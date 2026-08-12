use std::fs::{self, File};
use std::hint::black_box;
use std::io::Read;
use std::time::Instant;

const CACHE_SIZE: usize = 64 * 1024;

macro_rules! bench {
    ($name:expr, $iterations:expr, $body:expr) => {{
        for _ in 0..100 {
            black_box($body);
        }
        let mut samples = [0u128; 9];
        for sample in &mut samples {
            let start = Instant::now();
            for _ in 0..$iterations {
                black_box($body);
            }
            *sample = start.elapsed().as_nanos() / $iterations;
        }
        samples.sort_unstable();
        println!(
            "{:<28} {:>9} ns/op median ({:>9} best)",
            $name, samples[4], samples[0]
        );
    }};
}

fn atom(kind: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut output = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    output.extend_from_slice(kind);
    output.extend(payload);
    output
}

/// One chunk holding one sample of `size` bytes, starting at `offset`.
fn sample_tables(offset: u32, size: u32) -> Vec<u8> {
    let mut stco = vec![0; 4];
    stco.extend_from_slice(&1u32.to_be_bytes());
    stco.extend_from_slice(&offset.to_be_bytes());
    let mut stsz = vec![0; 4];
    stsz.extend_from_slice(&0u32.to_be_bytes());
    stsz.extend_from_slice(&1u32.to_be_bytes());
    stsz.extend_from_slice(&size.to_be_bytes());
    let mut output = atom(b"stco", stco);
    output.extend(atom(b"stsz", stsz));
    output
}

fn fixture(codec_private: bool) -> Vec<u8> {
    fixture_with(codec_private, Vec::new())
}

fn fixture_with(codec_private: bool, tables: Vec<u8>) -> Vec<u8> {
    let mut ftyp = b"isom\0\0\0\0isom".to_vec();
    let mut tkhd = vec![0; 84];
    tkhd[3] = 3;
    tkhd[40..44].copy_from_slice(&(1i32 << 16).to_be_bytes());
    tkhd[56..60].copy_from_slice(&(1i32 << 16).to_be_bytes());
    tkhd[76..80].copy_from_slice(&(640u32 << 16).to_be_bytes());
    tkhd[80..84].copy_from_slice(&(360u32 << 16).to_be_bytes());
    let mut sample = vec![0; 78];
    sample[24..26].copy_from_slice(&640u16.to_be_bytes());
    sample[26..28].copy_from_slice(&360u16.to_be_bytes());
    if codec_private {
        let sps = [
            0x67, 0x64, 0, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0, 0, 3, 0,
            4, 0, 0, 3, 0, 0xf1, 0x83, 0x19, 0x60,
        ];
        let mut avcc = vec![1, 0x64, 0, 0x28, 0xff, 0xe1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&sps);
        sample.extend(atom(b"avcC", avcc));
    }
    let mut stsd = vec![0; 8];
    stsd[7] = 1;
    stsd.extend(atom(b"avc1", sample));
    let mut stbl_payload = atom(b"stsd", stsd);
    stbl_payload.extend(tables);
    let stbl = atom(b"stbl", stbl_payload);
    let mut hdlr = vec![0; 12];
    hdlr[8..12].copy_from_slice(b"vide");
    let mut mdia = atom(b"hdlr", hdlr);
    mdia.extend(atom(b"minf", stbl));
    let mut trak = atom(b"tkhd", tkhd);
    trak.extend(atom(b"mdia", mdia));
    let mut output = atom(b"ftyp", std::mem::take(&mut ftyp));
    output.extend(atom(b"moov", atom(b"trak", trak)));
    output
}

fn main() {
    let mut metadata = fixture(false);
    metadata.extend(atom(b"mdat", vec![0; 128 * 1024]));
    let mut data = fixture(true);
    data.extend(atom(b"mdat", vec![0; 128 * 1024]));
    let path = std::env::temp_dir().join(format!("videosize-bench-{}", std::process::id()));
    fs::write(&path, &data).unwrap();

    // A first sample the size of a real keyframe, which every probe reaches for
    // to refine render size and pixel aspect.
    const KEYFRAME: usize = 128 * 1024;
    let metadata_size = fixture_with(true, sample_tables(0, KEYFRAME as u32)).len();
    let mut sampled = fixture_with(
        true,
        sample_tables((metadata_size + 8) as u32, KEYFRAME as u32),
    );
    sampled.extend(atom(b"mdat", vec![0; KEYFRAME]));
    let sampled_path =
        std::env::temp_dir().join(format!("videosize-bench-sample-{}", std::process::id()));
    fs::write(&sampled_path, &sampled).unwrap();

    println!("64 KiB cache construction");
    bench!("Box<[u8; 64 KiB]>", 10_000, Box::new([0u8; CACHE_SIZE]));
    bench!(
        "Vec -> Box<[u8]>",
        10_000,
        vec![0u8; CACHE_SIZE].into_boxed_slice()
    );
    bench!("Vec<u8>", 10_000, vec![0u8; CACHE_SIZE]);
    bench!("zero + one read", 2_000, {
        let mut file = File::open(&path).unwrap();
        let mut cache = vec![0u8; CACHE_SIZE];
        file.read(&mut cache).unwrap()
    });
    bench!("capacity + read_to_end", 2_000, {
        let file = File::open(&path).unwrap();
        let mut cache = Vec::with_capacity(CACHE_SIZE);
        file.take(CACHE_SIZE as u64)
            .read_to_end(&mut cache)
            .unwrap()
    });

    println!("\ncomplete public operations");
    bench!(
        "video_type(slice)",
        100_000,
        videosize::video_type(&data).unwrap()
    );
    bench!(
        "blob_probe(metadata)",
        10_000,
        videosize::blob_probe(&metadata).unwrap()
    );
    bench!(
        "blob_probe(avcC)",
        10_000,
        videosize::blob_probe(&data).unwrap()
    );
    bench!(
        "file_type(open + probe)",
        2_000,
        videosize::file_type(File::open(&path).unwrap()).unwrap()
    );
    bench!(
        "probe(open + probe)",
        2_000,
        videosize::probe(File::open(&path).unwrap()).unwrap()
    );
    bench!(
        "blob_probe(128 KiB sample)",
        10_000,
        videosize::blob_probe(&sampled).unwrap()
    );
    bench!(
        "probe(128 KiB sample file)",
        2_000,
        videosize::probe(File::open(&sampled_path).unwrap()).unwrap()
    );

    fs::remove_file(path).unwrap();
    fs::remove_file(sampled_path).unwrap();
}
