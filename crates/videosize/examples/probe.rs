fn main() {
    for path in std::env::args_os().skip(1) {
        match std::fs::File::open(&path)
            .map_err(videosize::VideoError::from)
            .and_then(videosize::probe)
        {
            Ok(info) => println!(
                "{}: {}x{}, {:?} {:?}, aspect {}, rotation {}°",
                std::path::Path::new(&path).display(),
                info.size.width,
                info.size.height,
                info.video_type,
                info.codec,
                info.aspect_ratio(),
                info.rotation,
            ),
            Err(error) => eprintln!("{}: {error}", std::path::Path::new(&path).display()),
        }
    }
}
