//! Example of using softbuffer with drm-rs.

#[cfg(kms_platform)]
mod imple {
    use drm::control::{connector, Device as CtrlDevice, Event, ModeTypeFlags, PlaneType};
    use drm::Device;

    use raw_window_handle::{DisplayHandle, DrmDisplayHandle, DrmWindowHandle, WindowHandle};
    use softbuffer::{Context, Surface};

    use std::num::NonZeroU32;
    use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd};
    use std::path::Path;
    use std::time::Instant;

    pub(super) fn entry() -> Result<(), Box<dyn std::error::Error>> {
        // Open a new device.
        let device = Card::find()?;

        // Create the softbuffer context.
        let context = unsafe {
            Context::new(DisplayHandle::borrow_raw({
                let handle = DrmDisplayHandle::new(device.as_fd().as_raw_fd());
                handle.into()
            }))
        }?;

        // Get the DRM handles.
        let handles = device.resource_handles()?;

        // Get the list of connectors and CRTCs.
        let connectors = handles
            .connectors()
            .iter()
            .map(|&con| device.get_connector(con, true))
            .collect::<Result<Vec<_>, _>>()?;
        let crtcs = handles
            .crtcs()
            .iter()
            .map(|&crtc| device.get_crtc(crtc))
            .collect::<Result<Vec<_>, _>>()?;

        // Find a connected crtc.
        let con = connectors
            .iter()
            .find(|con| con.state() == connector::State::Connected)
            .ok_or("No connected connectors")?;

        // Get the first CRTC.
        let crtc = crtcs.first().ok_or("No CRTCs")?;

        // Find a mode to use.
        let mode = con
            .modes()
            .iter()
            .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| con.modes().first())
            .ok_or("No modes")?;

        // Look for a primary plane compatible with our CRTC.
        let planes = device.plane_handles()?;
        let planes = planes
            .iter()
            .filter(|&&plane| {
                device.get_plane(plane).is_ok_and(|plane| {
                    let crtcs = handles.filter_crtcs(plane.possible_crtcs());
                    crtcs.contains(&crtc.handle())
                })
            })
            .collect::<Vec<_>>();

        // Find the first primary plane or take the first one period.
        let plane = planes
            .iter()
            .find(|&&&plane| {
                if let Ok(props) = device.get_properties(plane) {
                    let (ids, vals) = props.as_props_and_values();
                    for (&id, &val) in ids.iter().zip(vals.iter()) {
                        if let Ok(info) = device.get_property(id) {
                            if info.name().to_str() == Ok("type") {
                                return val == PlaneType::Primary as u32 as u64;
                            }
                        }
                    }
                }

                false
            })
            .or(planes.first())
            .ok_or("No planes")?;

        // Create the surface on top of this plane.
        // Note: This requires root on DRM/KMS.
        let mut surface = unsafe {
            Surface::new(
                &context,
                WindowHandle::borrow_raw({
                    let handle = DrmWindowHandle::new((**plane).into());
                    handle.into()
                }),
            )
        }?;

        // Resize the surface.
        let (width, height) = mode.size();
        surface.resize(
            NonZeroU32::new(width as u32).unwrap(),
            NonZeroU32::new(height as u32).unwrap(),
        )?;

        // Start drawing to it.
        let mut tick = 0;
        loop {
            tick += 1;
            let frame_start = Instant::now();

            // Start drawing.
            let draw_start = Instant::now();
            let mut buffer = surface.buffer_mut()?;
            draw_to_buffer(&mut buffer, tick);
            buffer.present()?;
            let draw_end = draw_start.elapsed();

            // Wait for the page flip to happen.
            let poll_start = Instant::now();
            rustix::event::poll(
                &mut [rustix::event::PollFd::new(
                    &device,
                    rustix::event::PollFlags::IN,
                )],
                None,
            )?;
            let poll_end = poll_start.elapsed();

            // Receive the events.
            let events = device.receive_events()?;
            for event in events {
                match event {
                    Event::PageFlip(_) => {}
                    Event::Vblank(_) => {
                        println!("Vblank event.");
                    }
                    _ => {
                        println!("Unknown event.");
                    }
                }
            }

            println!(
                "Frame {tick}, {draw_end:?} drawing, {poll_end:?} waiting, done in {:?}",
                frame_start.elapsed()
            );
        }
    }

    fn draw_to_buffer(buf: &mut [u32], tick: usize) {
        let screen_width = 800;
        let screen_height = 480;
        let yellow = 0xffffff99;
        let blue = 0xff386cb0;

        let sine = (tick as f32 / 30.0).cos();
        let start_x =
            ((sine + 1.0) * (screen_width as f32 / 2.1)).clamp(0.0, screen_width as f32) as usize;
        let start_y =
            ((sine + 1.0) * (screen_height as f32 / 2.1)).clamp(0.0, screen_height as f32) as usize;
        println!("Frame {tick}, x: {start_x}, y: {start_y}");

        buf.fill(yellow);

        for x in 0..screen_width {
            buf[x + start_y * screen_width] = blue;
        }
        for row in buf.chunks_mut(screen_width) {
            row[start_x] = blue;
        }
    }

    struct Card(std::fs::File);

    impl Card {
        fn find() -> Result<Card, Box<dyn std::error::Error>> {
            for i in 0..10 {
                let path = format!("/dev/dri/card{i}");
                // Card enumeration may not start at zero, allow failures while opening
                let Ok(device) = Card::open(path) else {
                    continue;
                };

                // Only use it if it has connectors.
                let Ok(handles) = device.resource_handles() else {
                    continue;
                };

                if handles
                    .connectors
                    .iter()
                    .filter_map(|c| device.get_connector(*c, false).ok())
                    .any(|c| c.state() == connector::State::Connected)
                {
                    return Ok(device);
                }
            }

            Err("No DRM device found".into())
        }

        fn open(path: impl AsRef<Path>) -> Result<Card, Box<dyn std::error::Error>> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?;
            Ok(Card(file))
        }
    }

    impl AsFd for Card {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }

    impl Device for Card {}
    impl CtrlDevice for Card {}
}

#[cfg(not(kms_platform))]
mod imple {
    pub(super) fn entry() -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("This example requires the `kms` feature.");
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    imple::entry()
}
