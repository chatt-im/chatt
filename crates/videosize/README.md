# videosize

`videosize` finds encoded video dimensions and authoritative display aspect
ratios without decoding frames. It is dependency-free and supports complete
MP4/MOV, Matroska/WebM, and AVI inputs.

```rust
let file = std::fs::File::open("clip.mp4")?;
let size = videosize::size(file)?;
println!("{}x{}", size.width, size.height);

let file = std::fs::File::open("anamorphic.mkv")?;
let info = videosize::probe(file)?;
println!("codec: {:?}, display aspect: {}", info.codec, info.aspect_ratio());
# Ok::<(), videosize::VideoError>(())
```

For complete in-memory files, use `blob_size` and `blob_probe`. `file_type`
identifies an owned `std::fs::File`; `video_type` is the one API intentionally
designed to identify a container from partial header bytes. An owned file's
initial cursor is ignored.

`VideoInfo::size` and the result of `size` are encoded/container pixel
dimensions. `VideoInfo::display_aspect_ratio` (also returned by
`VideoInfo::aspect_ratio()`) applies container apertures or crops, pixel aspect,
codec cropping/render geometry, and supported quarter-turn transforms.

| Container | Metadata and bounded header support |
| --- | --- |
| MP4 / MOV | sample entries, `pasp`, fractional `clap`, `tapt`/`clef`, movie/track matrices, regular and fragmented first samples |
| Matroska / WebM | track flags, crop/display elements and units, projection roll, codec private data, blocks and all lacing modes |
| AVI | enabled stream selection, bitmap headers, `vprp`, and selected-stream chunks in `movi`/`rec ` lists |

AVC/H.264, HEVC/H.265, AV1, VP8, and VP9 headers can supply missing encoded
geometry, cropping, sample aspect, and render dimensions. Unsupported codecs
can still return container-derived geometry.

All probing APIs except `video_type` require complete inputs. Parser work is
bounded; an exceeded track, structure, nesting, inspected-byte, or codec-header
budget returns `VideoError::LimitExceeded`. Malformed or truncated media returns
`VideoError::CorruptedVideo`, while filesystem failures return
`VideoError::IoError`.
