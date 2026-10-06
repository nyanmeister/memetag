//! Open originals in the user's viewers; identify animation from the file data.
use crate::containers::{self, Kind};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command, Stdio};

#[derive(Debug, PartialEq, Eq)]
pub enum Viewer {
    Feh,
    Mpv,
}

impl Viewer {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Feh => "feh",
            Self::Mpv => "mpv",
        }
    }
    fn command(&self, path: &Path) -> Command {
        let mut command = Command::new(self.name());
        match self {
            Self::Feh => {
                command.args(["--scale-down", "--fullscreen"]);
            }
            // Reload this single-file playlist instead of seeking back inside
            // an APNG/WebP demuxer: loop-file triggers replay errors locally.
            Self::Mpv => {
                command.args([
                    "--loop-playlist=inf",
                    "--loop-file=no",
                    "--force-window=yes",
                    "--autofit-larger=100%x100%",
                    "--fullscreen",
                ]);
            }
        }
        // No shell interpolation; even names beginning with '-' remain filenames.
        command.arg("--").arg(path);
        command
    }
}

fn detect<R: BufRead + Seek>(mut input: R, video: bool) -> Result<Viewer, String> {
    if video {
        return Ok(Viewer::Mpv);
    }
    let mut header = [0u8; 16];
    input
        .read_exact(&mut header)
        .map_err(|e| format!("Read image header: {e}"))?;
    input.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let animated = match containers::sniff(&header) {
        Kind::Gif | Kind::Mp4 | Kind::Matroska => true,
        Kind::Png => image::codecs::png::PngDecoder::new(input)
            .map_err(|e| e.to_string())?
            .is_apng()
            .map_err(|e| e.to_string())?,
        Kind::WebP => image::codecs::webp::WebPDecoder::new(input)
            .map_err(|e| e.to_string())?
            .has_animation(),
        _ => false,
    };
    Ok(if animated { Viewer::Mpv } else { Viewer::Feh })
}

pub fn launch(path: &Path, video: bool) -> Result<(Viewer, Child), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Open original: {e}"))?;
    let viewer = detect(BufReader::new(file), video)?;
    let child = viewer
        .command(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Start {}: {e}", viewer.name()))?;
    Ok((viewer, child))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn routes_stills_and_real_animated_containers() {
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::WebP,
            image::ImageFormat::Jpeg,
        ] {
            let mut bytes = Cursor::new(Vec::new());
            image::RgbImage::from_pixel(16, 16, image::Rgb([12, 34, 56]))
                .write_to(&mut bytes, format)
                .unwrap();
            bytes.set_position(0);
            assert_eq!(detect(bytes, false).unwrap(), Viewer::Feh);
        }
        for bytes in [
            include_bytes!("../tests/fixtures/animated.png").as_slice(),
            include_bytes!("../tests/fixtures/animated.webp").as_slice(),
        ] {
            assert_eq!(detect(Cursor::new(bytes), false).unwrap(), Viewer::Mpv);
        }
        let mut gif = Cursor::new(Vec::new());
        image::RgbImage::new(16, 16)
            .write_to(&mut gif, image::ImageFormat::Gif)
            .unwrap();
        gif.set_position(0);
        assert_eq!(detect(gif, false).unwrap(), Viewer::Mpv);
        assert_eq!(
            detect(Cursor::new(Vec::<u8>::new()), true).unwrap(),
            Viewer::Mpv
        );
        assert!(detect(Cursor::new(b"broken"), false).is_err());
    }

    #[test]
    fn viewer_arguments_loop_and_preserve_literal_paths() {
        let path = Path::new("/memes/a space; $(literal) --name.webp");
        let mpv = Viewer::Mpv.command(path);
        let args: Vec<_> = mpv.get_args().collect();
        assert_eq!(args[0], "--loop-playlist=inf");
        assert_eq!(args[1], "--loop-file=no");
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args.last().unwrap(), &path.as_os_str());
        let feh = Viewer::Feh.command(path);
        assert_eq!(feh.get_args().next().unwrap(), "--scale-down");
    }
}
