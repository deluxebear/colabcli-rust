//! Local image presentation; successful automatic files are retained user artifacts.
use crate::{Error, Result, error::io};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits};
use serde_json::Value;
use std::{
    borrow::Cow,
    io::{Cursor, IsTerminal, Write},
    path::{Path, PathBuf},
};

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const MAX_JPEG_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIMENSION: u32 = 8192;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PNG_BYTES: usize = 64 * 1024 * 1024;

struct PngOutput(Vec<u8>);
impl Write for PngOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_PNG_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("终端 PNG 超过大小限制"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// JPEG is not a Kitty f=100 payload: transcode the presentation only.
/// Limits bound input, dimensions, decoded pixel buffer and encoded output;
/// they are not a hard cap on all allocations inside the codec.
pub fn jpeg_to_png(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() > MAX_JPEG_BYTES {
        return Err(Error::KernelProtocol("JPEG 超过 8 MiB 显示限制"));
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Jpeg);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|_| Error::KernelProtocol("JPEG 尺寸或编码无效"))?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS
        || decoder.total_bytes() > MAX_DECODED_BYTES
    {
        return Err(Error::KernelProtocol("JPEG 像素数或解码大小超过显示限制"));
    }
    let image =
        DynamicImage::from_decoder(decoder).map_err(|_| Error::KernelProtocol("JPEG 解码失败"))?;
    let mut output = PngOutput(Vec::new());
    image::codecs::png::PngEncoder::new(&mut output)
        .write_image(image.as_bytes(), width, height, image.color().into())
        .map_err(|_| Error::KernelProtocol("JPEG 转 PNG 失败或结果超过显示限制"))?;
    Ok(output.0)
}

/// Emit one PNG using Kitty direct transmission, with at most 4096 base64 bytes
/// per packet. q=2 prevents terminal replies from becoming REPL input.
/// The caller decides whether the output is a terminal and supplies PNG bytes.
pub fn write_kitty_png(writer: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    let encoded = STANDARD.encode(bytes);
    if encoded.is_empty() {
        return Ok(());
    }
    writer.write_all(b"\n")?;
    let chunks = encoded.as_bytes().chunks(4096);
    let count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        if index == 0 {
            write!(
                writer,
                "\x1b_Ga=T,f=100,q=2,m={};",
                u8::from(index + 1 < count)
            )?;
        } else {
            write!(writer, "\x1b_Gm={};", u8::from(index + 1 < count))?;
        }
        writer.write_all(chunk)?;
        writer.write_all(b"\x1b\\")?;
    }
    writer.write_all(b"\n")?;
    writer.flush()
}

fn decode(data: &Value) -> Result<Option<(Vec<u8>, &'static str)>> {
    for (mime, extension) in [("image/png", "png"), ("image/jpeg", "jpeg")] {
        let Some(encoded) = data
            .get(mime)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let bytes = STANDARD
            .decode(super::image_base64_bytes(encoded))
            .map_err(|_| Error::KernelProtocol("图片 base64 无效"))?;
        return Ok(Some((bytes, extension)));
    }
    Ok(None)
}

fn retain(bytes: &[u8], extension: &str) -> Result<PathBuf> {
    let mut file = tempfile::Builder::new()
        .prefix("colab-image-")
        .suffix(&format!(".{extension}"))
        .tempfile()
        .map_err(|error| io("临时图片", error))?;
    let path = file.path().to_owned();
    file.write_all(bytes).map_err(|error| io(&path, error))?;
    file.as_file()
        .sync_all()
        .map_err(|error| io(&path, error))?;
    let (file, path) = file.keep().map_err(|error| io("保留图片", error.error))?;
    drop(file);
    Ok(path)
}

/// Best-effort image decoding/storage must not make executed code uncertain.
/// An explicit target is saved by the caller after execution using save_last_image.
/// Without one, every displayed image (including updates) retains its own file,
/// matching upstream's artifact lifetime. The user/OS owns eventual deletion.
pub fn render(data: &Value, explicit_target: Option<&Path>) -> Result<()> {
    let (bytes, extension) = match decode(data) {
        Ok(Some(image)) => image,
        Ok(None) => return Ok(()),
        Err(error) => {
            eprintln!("图片显示失败：{error}");
            return Ok(());
        }
    };
    let saved = if explicit_target.is_none() {
        match retain(&bytes, extension) {
            Ok(path) => Some(path),
            Err(error) => {
                eprintln!("图片保存失败：{error}");
                None
            }
        }
    } else {
        None
    };
    let stdout = std::io::stdout();
    let tty = stdout.is_terminal();
    let png: Option<Cow<'_, [u8]>> = if !tty {
        None
    } else if extension == "jpeg" {
        match jpeg_to_png(&bytes) {
            Ok(png) => Some(Cow::Owned(png)),
            Err(error) => {
                eprintln!("图片显示失败：{error}");
                None
            }
        }
    } else if bytes.starts_with(PNG_SIGNATURE) {
        Some(Cow::Borrowed(&bytes))
    } else {
        eprintln!("PNG 图片标记无效；跳过终端显示");
        None
    };
    let mut writer = stdout.lock();
    if let Some(png) = png {
        write_kitty_png(&mut writer, &png).map_err(|error| io("stdout", error))?;
    }
    if let Some(path) = saved {
        writeln!(writer, "\n[Image saved to: {}]", path.display())
            .map_err(|error| io("stdout", error))?;
        writer.flush().map_err(|error| io("stdout", error))?;
    }
    Ok(())
}
