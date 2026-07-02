type Ret<T> = Result<T, Box<dyn std::error::Error>>;

fn name_of(file: &str) -> &str {
    let (_, file) = file.rsplit_once('/').unwrap_or(("", file));
    let file = file.strip_prefix("iconmonstr-").unwrap_or(file);
    let file = file.strip_suffix(".svg").unwrap_or(file);
    let file = file.strip_suffix("-1").unwrap_or(file);
    file
}
fn torify_svg(buf: &mut Vec<u8>, file: &str) -> Ret<()> {
    let bytes = std::fs::read(file)?;
    let prefix = b"path d=\"";
    let start = memchr::memmem::find(&bytes, prefix).ok_or("missing path d")?;
    let path = &bytes[start + prefix.len()..];
    let end = memchr::memchr(b'"', &path).ok_or("missing path d")?;
    let path = &path[..end];
    buf.push(b'.');
    buf.extend_from_slice(name_of(file).as_bytes());
    buf.extend_from_slice(b"-ico:after {\n    clip-path: path(\"");
    buf.extend_from_slice(path);
    buf.extend_from_slice(b"\");\n}\n");
    Ok(())
}

fn torify_directory(buf: &mut Vec<u8>, dir: &str) -> Ret<()> {
    for entry in std::fs::read_dir(dir)?.filter_map(|x| x.ok()) {
        let raw_path = entry.path();
        let path = raw_path.as_os_str().to_string_lossy();
        if !path.ends_with(".svg") {
            continue;
        }
        torify_svg(buf, &path).map_err(|err| format!("{path}{err}"))?;
    }
    Ok(())
}

fn main() {
    let mut args = std::env::args();
    args.next();
    let directory = args
        .next()
        .expect("Missing CLI Argument 1: Input Directory");
    let output = args.next().expect("Missing CLI Argument 2: Output File");
    let mut buf = Vec::<u8>::with_capacity(4096 * 16);
    torify_directory(&mut buf, &directory).unwrap();
    std::fs::write(&output, &buf).expect("failed to write output file");
}
