#[macro_use]
extern crate log;

use anyhow::Result;

use basic_frame::{match_all_dynamic_fmts, DynamicFrame};
use ci2_remote_control::{Mp4RecordingConfig, NvidiaH264Options, OpenH264Options};
use clap::Parser;
use convert_image::{encode_y4m_frame, ImageOptions, Y4MColorspace};
use machine_vision_formats::{
    pixel_format, pixel_format::PixFmt, ImageBuffer, ImageBufferRef, ImageData, Stride,
};
use std::path::{Path, PathBuf};
use timestamped_frame::ExtraTimeData;

const Y4M_MAGIC: &str = "YUV4MPEG2";
const Y4M_FRAME_MAGIC: &str = "FRAME";

/*

Examples of exporting from FMF to MKV with `ffv1` codec. Note these all loose
timestamp data:

    fmf export-y4m test_rgb8.fmf -o - | ffmpeg -i - -vcodec ffv1 test_yuv.mkv

Example export to mkv for mono8. Will lose timestamps:

    fmf export-y4m test_mono8.fmf -o - | ffmpeg -i - -vcodec ffv1 test_mono8.mkv

Example export to mkv via RGB (should be lossless for image data). Will loose timestamps:

    fmf export-bgr24 test_rgb8.fmf -o - | ffmpeg -i - -f rawvideo -pix_fmt bgr0 -s 332x332 -r 30 -vcodec ffv1 test_bgr.mkv

Note that MKV's `DateUTC` metadata creation time can be set when creating an MKV
video in ffmpeg with the option `-metadata creation_time="2012-02-07 12:15:27"`.
However, as of the time of writing, ffmpeg only parses the command line date to
the second (whereas the MKV spec allows better precision).

Example export to mp4:

    fmf export-mp4 test_rgb8.fmf -o /tmp/test.mp4

*/

/// Convert to runtime specified pixel format and save to FMF file.
macro_rules! convert_and_write_fmf {
    ($new_pixel_format:expr, $writer:expr, $x:expr, $timestamp:expr) => {{
        use pixel_format::*;
        match $new_pixel_format {
            PixFmt::Mono8 => write_converted!(Mono8, $writer, $x, $timestamp),
            PixFmt::Mono32f => write_converted!(Mono32f, $writer, $x, $timestamp),
            PixFmt::RGB8 => write_converted!(RGB8, $writer, $x, $timestamp),
            PixFmt::BayerRG8 => write_converted!(BayerRG8, $writer, $x, $timestamp),
            PixFmt::BayerRG32f => write_converted!(BayerRG32f, $writer, $x, $timestamp),
            PixFmt::BayerGB8 => write_converted!(BayerGB8, $writer, $x, $timestamp),
            PixFmt::BayerGB32f => write_converted!(BayerGB32f, $writer, $x, $timestamp),
            PixFmt::BayerGR8 => write_converted!(BayerGR8, $writer, $x, $timestamp),
            PixFmt::BayerGR32f => write_converted!(BayerGR32f, $writer, $x, $timestamp),
            PixFmt::BayerBG8 => write_converted!(BayerBG8, $writer, $x, $timestamp),
            PixFmt::BayerBG32f => write_converted!(BayerBG32f, $writer, $x, $timestamp),
            PixFmt::YUV422 => write_converted!(YUV422, $writer, $x, $timestamp),
            _ => {
                anyhow::bail!("unsupported pixel format {}", $new_pixel_format);
            }
        }
    }};
}

/// For a specified runtime specified pixel format, convert and save to FMF file.
macro_rules! write_converted {
    ($pixfmt:ty, $writer:expr, $x:expr, $timestamp:expr) => {{
        let converted_frame = convert_image::convert::<_, $pixfmt>($x)?;
        $writer.write(&converted_frame, $timestamp)?;
    }};
}

#[derive(Debug, Parser)]
#[command(name = "fmf", about, version)]
enum Opt {
    /// export an fmf file
    ExportFMF {
        /// new pixel_format (default: no change from input fmf)
        #[arg(long)]
        new_pixel_format: Option<PixFmt>,

        /// force input data to be interpreted with this pixel_format
        #[arg(long)]
        forced_input_pixel_format: Option<PixFmt>,

        /// Filename of input fmf
        input: PathBuf,

        /// Filename of output .fmf, "-" for stdout
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// print information about an fmf file
    Info {
        /// Filename of input fmf
        input: PathBuf,
    },

    /// export a sequence of jpeg images
    ExportJpeg {
        /// Filename of input fmf
        input: PathBuf,

        /// Quality (1-100 where 1 is the worst and 100 is the best)
        #[arg(short, long, default_value = "99")]
        quality: u8,
    },

    /// export a sequence of png images
    ExportPng {
        /// Filename of input fmf
        input: PathBuf,
    },

    /// export to y4m (YUV4MPEG2) format
    ExportY4m(ExportY4m),

    /// export to mp4
    ExportMp4(ExportMp4),

    /// import a sequence of images, converting it to an FMF file
    ImportImages {
        /// Input images (glob pattern like "*.png")
        input: String,

        /// Filename of output fmf
        #[arg(short, long)]
        output: PathBuf,
    },
}

#[derive(Parser, Debug)]
struct ExportY4m {
    /// Filename of input fmf
    input: PathBuf,

    /// Filename of output .y4m, "-" for stdout
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// colorspace (e.g. 420paldv, mono)
    #[arg(short, long, default_value = "420paldv")]
    colorspace: Y4MColorspace,

    /// frames per second numerator
    #[arg(long, default_value = "25")]
    fps_numerator: u32,

    /// frames per second denominator
    #[arg(long, default_value = "1")]
    fps_denominator: u32,

    /// aspect ratio numerator
    #[arg(long, default_value = "1")]
    aspect_numerator: u32,

    /// aspect ratio denominator
    #[arg(long, default_value = "1")]
    aspect_denominator: u32,
}

#[derive(Parser, Debug)]
struct ExportMp4 {
    /// Filename of input fmf
    input: PathBuf,

    /// Filename of output .mp4, "-" for stdout
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// video bitrate
    #[arg(short, long)]
    bitrate: Option<u32>,

    /// video codec
    #[arg(long, default_value = "vp9", help=VALID_CODECS)]
    codec: Codec,
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum Codec {
    NvencH264,
    OpenH264,
}

const VALID_CODECS: &str = "Codec must be one of: nvenc-h264 open-h264";

impl std::str::FromStr for Codec {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let s = s.to_lowercase();
        match s.as_str() {
            "nvenc-h264" => Ok(Codec::NvencH264),
            "open-h264" => Ok(Codec::OpenH264),
            c => Err(format!("unknown codec: {} ({})", c, VALID_CODECS)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Autocrop {
    None,
    Even,
    Mod16,
}

impl std::str::FromStr for Autocrop {
    type Err = &'static str;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "None" | "none" => Ok(Autocrop::None),
            "Even" | "even" => Ok(Autocrop::Even),
            "Mod16" | "mod16" => Ok(Autocrop::Mod16),
            _ => Err("unknown autocrop"),
        }
    }
}

/// convert None into default name, convert "-" into None (for stdout)
fn default_filename(path: &Path, output: Option<PathBuf>, ext: &str) -> Option<PathBuf> {
    match output {
        Some(x) => {
            if x.to_str() == Some("-") {
                None
            } else {
                Some(x)
            }
        }
        None => {
            let mut stem = path.file_stem().unwrap().to_os_string(); // strip extension
            stem.push(format!(".exported.{}", ext));
            Some(path.with_file_name(&stem))
        }
    }
}

fn display_filename(p: &Option<PathBuf>, default: &str) -> PathBuf {
    match p {
        Some(x) => x.clone(),
        None => std::path::Path::new(default).to_path_buf(),
    }
}

fn info(path: PathBuf) -> Result<()> {
    #[derive(Debug)]
    #[allow(dead_code)]
    struct Info {
        width: u32,
        height: u32,
        stride: usize,
        pixel_format: PixFmt,
    }
    let reader = fmf::FMFReader::new(&path)?;
    for (fno, frame) in reader.enumerate() {
        let frame = frame?;
        let i = Info {
            width: frame.width(),
            stride: frame.stride(),
            height: frame.height(),
            pixel_format: frame.pixel_format(),
        };
        if fno == 0 {
            println!("{:?}", i);
        }
        println!("frame {}: {}", fno, frame.extra().host_timestamp());
    }
    Ok(())
}

/// Write an fmf file
///
/// If the `forced_input_pixel_format` argument is not None, it forces the
/// interpretation of the original data into this format regardless of the pixel
/// format specied in the header of the input file.
fn export_fmf(
    path: PathBuf,
    new_pixel_format: Option<PixFmt>,
    output: Option<PathBuf>,
    forced_input_pixel_format: Option<PixFmt>,
) -> Result<()> {
    let output_fname = default_filename(&path, output, "fmf");

    info!(
        "exporting {} to {}",
        path.display(),
        display_filename(&output_fname, "<stdout>").display()
    );
    let reader = fmf::FMFReader::new(&path)?;

    let output_fname = output_fname.unwrap(); // XXX temp hack FIXME

    let f = std::fs::File::create(&output_fname)?;
    let mut writer = fmf::FMFWriter::new(f)?;

    for frame in reader {
        let frame = frame?;
        let fts = frame.extra().host_timestamp();
        let frame: DynamicFrame = match forced_input_pixel_format {
            Some(forced_input_pixel_format) => frame.force_pixel_format(forced_input_pixel_format),
            None => frame,
        };

        let fmt = match new_pixel_format {
            Some(new_pixel_format) => new_pixel_format,
            None => frame.pixel_format(),
        };

        match_all_dynamic_fmts!(frame, x, convert_and_write_fmf!(fmt, writer, &x, fts));
    }
    Ok(())
}

fn import_images(pattern: &str, output_fname: PathBuf) -> Result<()> {
    let opts = glob::MatchOptions::new();
    let paths = glob::glob_with(pattern, opts)?;
    let f = std::fs::File::create(&output_fname)?;
    let mut writer = fmf::FMFWriter::new(f)?;

    for path in paths {
        let piston_image = image::open(&path?)?;
        let converted_frame = convert_image::piston_to_frame(piston_image)?;
        writer.write(&converted_frame, chrono::Utc::now())?;
    }
    Ok(())
}

fn export_images(path: PathBuf, opts: ImageOptions) -> Result<()> {
    use std::io::Write;

    let stem = path.file_stem().unwrap().to_os_string(); // strip extension
    let dirname = path.with_file_name(&stem);

    let ext = match opts {
        ImageOptions::Jpeg(_) => "jpg",
        ImageOptions::Png => "png",
    };

    info!("saving {} images to {}", ext, dirname.display());

    match std::fs::create_dir(&dirname) {
        Ok(()) => {}
        Err(e) => match e.kind() {
            std::io::ErrorKind::AlreadyExists => {}
            _ => {
                return Err(e.into());
            }
        },
    }

    let reader = fmf::FMFReader::new(&path)?;

    for (i, frame) in reader.enumerate() {
        let frame = frame?;
        let file = format!("frame{:05}.{}", i, ext);
        let fname = dirname.join(&file);
        let buf = match_all_dynamic_fmts!(frame, x, convert_image::frame_to_image(&x, opts))?;
        let mut fd = std::fs::File::create(fname)?;
        fd.write_all(&buf)?;
    }
    Ok(())
}

// fn do_autocrop(w: usize, h: usize, autocrop: Autocrop) -> (usize,usize) {
//     match autocrop {
//         Autocrop::None => (w,h),
//         Autocrop::Even => ((w/2)*2,h),
//         Autocrop::Mod16 => ((w/16)*16,(h/16)*16),
//     }
// }

// fn encode_bgr24_frame( frame: fmf::DynamicFrame, autocrop: Autocrop ) -> fmf::FMFResult<Vec<u8>> {
//     use PixFmt::*;

//     // convert bayer formats
//     let frame: ConvertImageFrame = match frame.pixel_format() {
//         BayerRG8 | BayerGB8 | BayerGR8 | BayerBG8 => {
//             convert_image::bayer_to_rgb(&frame).unwrap()
//         }
//         _ => {
//             frame.into()
//         }
//     };

//     match frame.pixel_format() {
//         // MONO8 => {
//         //     // Should we set RGB each to mono? Or use conversion YUV to RGB?
//         //     unimplemented!();
//         // }
//         RGB8 => {
//             let w = frame.width() as usize;
//             let h = frame.height() as usize;
//             let (w,h) = do_autocrop(w,h,autocrop);
//             let mut buf: Vec<u8> = Vec::with_capacity(w*h*3);
//             for i in 0..h {
//                 let rowidx = i*frame.stride();
//                 for j in 0..w {
//                     let colidx = j*3;
//                     let start = rowidx + colidx;
//                     let stop = start+3;
//                     let rgb = &frame.image_data()[start..stop];
//                     let b = rgb[2];
//                     let g = rgb[1];
//                     let r = rgb[0];
//                     buf.push(b);
//                     buf.push(g);
//                     buf.push(r);
//                 }
//             }
//             Ok(buf)
//         }
//         fmt => {
//             Err(fmf::FMFError::UnimplementedPixelFormat(fmt))
//         }
//     }
// }

// fn export_bgr24(x: ExportBgr24) -> Result<()> {
//     use std::io::Write;

//     let output_fname = default_filename(&x.input, x.output, "bgr24");

//     let reader = fmf::FMFReader::new(&x.input)?;
//     let (w,h) = do_autocrop(reader.width() as usize, reader.height() as usize, x.autocrop);

//     info!("exporting {} ({}x{}) to {}", x.input.display(), w, h,
//         display_filename(&output_fname, "<stdout>").display());

//     let mut out_fd: Box<dyn Write> = match output_fname {
//         None => Box::new(std::io::stdout()),
//         Some(path) => Box::new(std::fs::File::create(&path)?),
//     };

//     for frame in reader {
//         let buf = encode_bgr24_frame( frame, x.autocrop )?;
//         out_fd.write_all(&buf)?;
//     }
//     out_fd.flush()?;
//     Ok(())
// }

fn export_mp4(x: ExportMp4) -> Result<()> {
    // TODO: read this https://www.webmproject.org/docs/encoder-parameters/
    // also this https://www.webmproject.org/docs/webm-sdk/example_vp9_lossless_encoder.html

    let output_fname = default_filename(&x.input, x.output, "mp4");

    info!(
        "exporting {} to {}",
        x.input.display(),
        display_filename(&output_fname, "<stdout>").display()
    );

    let out_fd = match &output_fname {
        None => {
            anyhow::bail!("Cannot export mp4 to stdout."); // Seek required
        }
        Some(path) => std::fs::File::create(&path)?,
    };

    let mut reader = fmf::FMFReader::new(&x.input)?;

    let libs = if x.codec == Codec::NvencH264 {
        Some(nvenc::Dynlibs::new()?)
    } else {
        None
    };

    let (codec, nv_enc) = match x.codec {
        Codec::NvencH264 => {
            let mut opts = NvidiaH264Options::default();
            if let Some(bitrate) = x.bitrate {
                opts.bitrate = bitrate;
            }
            let nv_enc = Some(nvenc::NvEnc::new(libs.as_ref().unwrap())?);
            (ci2_remote_control::Mp4Codec::H264NvEnc(opts), nv_enc)
        }
        Codec::OpenH264 => {
            let opts = match x.bitrate {
                None => OpenH264Options {
                    debug: false,
                    preset: ci2_remote_control::OpenH264Preset::AllFrames,
                },
                Some(bitrate) => OpenH264Options {
                    debug: false,
                    preset: ci2_remote_control::OpenH264Preset::SkipFramesBitrate(bitrate),
                },
            };
            dbg!(&opts);
            (ci2_remote_control::Mp4Codec::H264OpenH264(opts), None)
        }
    };

    // read first frames to get duration.
    const BUFSZ: usize = 50;
    let mut buffered_first = Vec::with_capacity(BUFSZ);
    while let Some(next) = reader.next() {
        buffered_first.push(Ok(next?));
        if buffered_first.len() >= BUFSZ {
            break;
        }
    }
    // collect timestamps
    let ts_first: Vec<_> = buffered_first
        .iter()
        .map(|res_frame| res_frame.as_ref().unwrap().extra().host_timestamp())
        .collect();
    // collect deltas
    let dt_first: Vec<f64> = ts_first
        .windows(2)
        .map(|tss| {
            assert_eq!(tss.len(), 2);
            dbg!(&tss);
            (tss[1] - tss[0]).to_std().unwrap().as_secs_f64()
        })
        .collect();
    dbg!(&dt_first);

    let cfg = Mp4RecordingConfig {
        codec,
        max_framerate: ci2_remote_control::RecordingFrameRate::Unlimited,
        h264_metadata: None,
    };

    debug!("opening file {}", output_fname.unwrap().display());
    let mut my_mp4_writer = mp4_writer::Mp4Writer::new(out_fd, cfg, nv_enc)?;

    for (fno, fmf_frame) in buffered_first.into_iter().chain(reader).enumerate() {
        let fmf_frame = fmf_frame?;
        debug!("saving frame {}", fno);
        let ts = fmf_frame.extra().host_timestamp();
        match_all_dynamic_fmts!(fmf_frame, frame, {
            my_mp4_writer.write(&frame, ts)?;
        });
    }

    debug!("finishing file");
    my_mp4_writer.finish()?;
    Ok(())
}

/// A view of a source image in which the rightmost pixels may be clipped
struct ClippedFrame<'a, FMT> {
    src: &'a basic_frame::BasicFrame<FMT>,
    width: u32,
}

impl<'a, FMT> ImageData<FMT> for ClippedFrame<'a, FMT> {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.src.height()
    }
    fn buffer_ref(&self) -> ImageBufferRef<'a, FMT> {
        ImageBufferRef::new(self.src.image_data())
    }
    fn buffer(self) -> ImageBuffer<FMT> {
        ImageBuffer::new(self.buffer_ref().data.to_vec()) // copy data
    }
}

impl<'a, FMT> Stride for ClippedFrame<'a, FMT> {
    fn stride(&self) -> usize {
        self.src.stride()
    }
}

trait ClipFrame<FMT> {
    fn clip_to_power_of_2(&self, val: u8) -> ClippedFrame<FMT>;
}

impl<FMT> ClipFrame<FMT> for basic_frame::BasicFrame<FMT> {
    fn clip_to_power_of_2(&self, val: u8) -> ClippedFrame<FMT> {
        let width = (self.width() / val as u32) * val as u32;
        debug!("clipping image of width {} to {}", self.width(), width);
        ClippedFrame { src: self, width }
    }
}

fn export_y4m(x: ExportY4m) -> Result<()> {
    use std::io::Write;

    let output_fname = default_filename(&x.input, x.output, "y4m");

    info!(
        "exporting {} to {}",
        x.input.display(),
        display_filename(&output_fname, "<stdout>").display()
    );

    let mut out_fd: Box<dyn Write> = match output_fname {
        None => Box::new(std::io::stdout()),
        Some(path) => Box::new(std::fs::File::create(&path)?),
    };

    let reader = fmf::FMFReader::new(&x.input)?;
    let mut buffer_width = reader.width();
    let buffer_height = reader.height();

    if reader.format() == PixFmt::RGB8 {
        buffer_width *= 3;
    }

    let final_width = match reader.format() {
        PixFmt::RGB8 => buffer_width / 3,
        _ => buffer_width,
    };
    let final_height = buffer_height;

    let inter = "Ip"; // progressive

    let buf = format!(
        "{magic} W{width} H{height} \
                    F{raten}:{rated} {inter} A{aspectn}:{aspectd} \
                    C{colorspace} XCOLORRANGE=FULL Xconverted_by-fmf-cli\n",
        magic = Y4M_MAGIC,
        width = final_width,
        height = final_height,
        raten = x.fps_numerator,
        rated = x.fps_denominator,
        inter = inter,
        aspectn = x.aspect_numerator,
        aspectd = x.aspect_denominator,
        colorspace = x.colorspace
    );
    out_fd.write_all(buf.as_bytes())?;

    for frame in reader {
        let frame = frame?;
        let buf = format!("{magic}\n", magic = Y4M_FRAME_MAGIC);
        out_fd.write_all(buf.as_bytes())?;

        basic_frame::match_all_dynamic_fmts!(frame, f, {
            let buf = encode_y4m_frame(&f, x.colorspace, None)?;
            out_fd.write_all(&buf.data)?;
        });
    }
    out_fd.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "fmf=info,warn");
    }

    env_logger::init();
    let opt = Opt::parse();

    match opt {
        Opt::ExportFMF {
            input,
            new_pixel_format,
            output,
            forced_input_pixel_format,
        } => {
            export_fmf(input, new_pixel_format, output, forced_input_pixel_format)?;
        }
        Opt::Info { input } => {
            info(input)?;
        }
        Opt::ExportJpeg { input, quality } => {
            export_images(input, ImageOptions::Jpeg(quality))?;
        }
        Opt::ExportPng { input } => {
            export_images(input, ImageOptions::Png)?;
        }
        Opt::ExportY4m(x) => {
            export_y4m(x)?;
        }
        // Opt::ExportBgr24(x) => {
        //     export_bgr24(x)?;
        // },
        Opt::ExportMp4(x) => {
            export_mp4(x)?;
        }
        Opt::ImportImages { input, output } => {
            import_images(&input, output)?;
        }
    }
    Ok(())
}

#[test]
fn test_y4m() -> anyhow::Result<()> {
    use machine_vision_formats::pixel_format::{Mono8, RGB8};

    let start = chrono::DateTime::from_timestamp(61, 0).unwrap();
    for output_colorspace in [Y4MColorspace::CMono, Y4MColorspace::C420paldv] {
        for input_colorspace in [PixFmt::Mono8, PixFmt::RGB8] {
            let tmpdir = tempfile::tempdir()?;
            let base_path = tmpdir.path().to_path_buf();
            println!("files in base_path: {}", base_path.display());
            std::mem::forget(tmpdir);

            const W: usize = 8;
            const STEP: u8 = 32;
            assert_eq!(W * STEP as usize, 256);

            let width: u32 = W.try_into().unwrap();
            let height = 4;

            let mut image_data = vec![0u8; W * height as usize];
            for row_data in image_data.chunks_exact_mut(W) {
                for (col, el) in row_data.iter_mut().enumerate() {
                    let col: u8 = col.try_into().unwrap();
                    *el = STEP * col;
                }
            }

            // make mono8 image. Will covert to input_colorspace below.
            let frame = basic_frame::BasicFrame {
                width,
                height,
                stride: width,
                pixel_format: std::marker::PhantomData::<Mono8>,
                image_data,
                extra: Box::new(basic_frame::BasicExtra {
                    host_timestamp: start,
                    host_framenumber: 0,
                }),
            };
            let orig_rgb8 = convert_image::convert::<_, RGB8>(&frame)?;

            let fmf_fname = base_path.join("test.fmf");
            let y4m_fname = base_path.join("test.y4m");
            {
                let fd = std::fs::File::create(&fmf_fname)?;
                let mut writer = fmf::FMFWriter::new(fd)?;

                match input_colorspace {
                    PixFmt::Mono8 => {
                        let converted_frame = convert_image::convert::<_, Mono8>(&frame)?;
                        writer.write(&converted_frame, start)?;
                    }
                    PixFmt::RGB8 => {
                        let converted_frame = convert_image::convert::<_, RGB8>(&frame)?;
                        writer.write(&converted_frame, start)?;
                    }
                    _ => {
                        todo!();
                    }
                }
            }

            let x = ExportY4m {
                input: fmf_fname,
                output: Some(y4m_fname.clone()),
                colorspace: output_colorspace,
                fps_numerator: 25,
                fps_denominator: 1,
                aspect_numerator: 1,
                aspect_denominator: 1,
            };

            export_y4m(x)?;

            let loaded = ffmpeg_to_frame(&y4m_fname, &base_path)?;
            let loaded: &dyn machine_vision_formats::ImageStride<_> = &loaded;
            let orig_rgb8: &dyn machine_vision_formats::ImageStride<_> = &orig_rgb8;
            println!("{input_colorspace:?} -> {output_colorspace:?}");
            for im in [loaded, orig_rgb8].iter() {
                println!("{:?}", im.image_data());
            }
            assert!(are_images_equal(loaded, orig_rgb8));
        }
    }
    Ok(())
}

#[cfg(test)]
fn are_images_equal<FMT>(
    frame1: &dyn machine_vision_formats::ImageStride<FMT>,
    frame2: &dyn machine_vision_formats::ImageStride<FMT>,
) -> bool
where
    FMT: machine_vision_formats::PixelFormat,
{
    let width = frame1.width();

    if frame1.width() != frame2.width() {
        return false;
    }
    if frame1.height() != frame2.height() {
        return false;
    }

    let fmt = machine_vision_formats::pixel_format::pixfmt::<FMT>().unwrap();
    let valid_stride = fmt.bits_per_pixel() as usize * width as usize / 8;

    for (f1_row, f2_row) in frame1
        .image_data()
        .chunks_exact(frame1.stride())
        .zip(frame2.image_data().chunks_exact(frame2.stride()))
    {
        let f1_valid = &f1_row[..valid_stride];
        let f2_valid = &f2_row[..valid_stride];
        if f1_valid != f2_valid {
            return false;
        }
    }

    true
}

#[cfg(test)]
fn ffmpeg_to_frame(
    fname: &std::path::Path,
    base_path: &std::path::Path,
) -> anyhow::Result<simple_frame::SimpleFrame<machine_vision_formats::pixel_format::RGB8>> {
    use anyhow::Context;

    let png_fname = base_path.join("frame1.png");
    let args = [
        "-i",
        &format!("{}", fname.display()),
        &format!("{}", png_fname.display()),
    ];
    let output = std::process::Command::new("ffmpeg")
        .args(&args)
        .output()
        .with_context(|| format!("When running: ffmpeg {:?}", args))?;

    if !output.status.success() {
        anyhow::bail!(
            "'ffmpeg {}' failed. stdout: {}, stderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let piston_image =
        image::open(&png_fname).with_context(|| format!("Opening {}", png_fname.display()))?;
    let decoded = convert_image::piston_to_frame(piston_image)?;
    Ok(decoded)
}
