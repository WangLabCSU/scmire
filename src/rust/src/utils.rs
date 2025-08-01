use std::fs::File;
use std::io::BufReader;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use extendr_api::prelude::*;
#[cfg(not(feature = "isal"))]
use flate2::bufread::GzDecoder;
use indicatif::style::TemplateError;
use indicatif::ProgressBar;
use indicatif::ProgressStyle;
#[cfg(feature = "isal")]
use isal::read::GzipDecoder;
use libdeflater::Compressor;
use memchr::memmem::Finder;

use crate::reader::*;

pub(crate) const BLOCK_SIZE: usize = 8 * 1024 * 1024;
pub(crate) const BUFFER_SIZE: usize = 4 * 1024 * 1024;

pub(crate) const TAG_PREFIX: &'static [u8] = b"MIRE{";
pub(crate) const TAG_SUFFIX: u8 = b'}';
pub(crate) static TAG_PREFIX_FINDER: std::sync::LazyLock<Finder> =
    std::sync::LazyLock::new(|| Finder::new(TAG_PREFIX));
pub(crate) const KOUTPUT_TAXID_PREFIX: &'static [u8] = b"(taxid ";
pub(crate) const KOUTPUT_TAXID_SUFFIX: u8 = b')';
pub(crate) static KOUTPUT_TAXID_PREFIX_FINDER: std::sync::LazyLock<Finder> =
    std::sync::LazyLock::new(|| Finder::new(KOUTPUT_TAXID_PREFIX));

// Parse &[u8] slice to f64 assuming ASCII decimal representation
pub(crate) fn parse_f64(bytes: &[u8]) -> Result<f64> {
    let s = str::from_utf8(bytes.trim_ascii())
        .with_context(|| format!("Invalid UTF-8: '{:?}'", bytes))?;
    s.parse::<f64>()
        .with_context(|| format!("Failed to parse float '{}'", s))
}

// Parse &[u8] slice to usize assuming ASCII decimal representation
pub(crate) fn parse_usize(bytes: &[u8]) -> Result<usize> {
    let s = str::from_utf8(bytes.trim_ascii())
        .with_context(|| format!("Invalid UTF-8: '{:?}'", bytes))?;
    s.parse::<usize>()
        .with_context(|| format!("Failed to parse integer '{}'", s))
}

pub(crate) fn u8_to_list_rstr(vv: Vec<Vec<u8>>) -> Vec<Rstr> {
    vv.into_iter().map(|v| u8_to_rstr(v)).collect()
}

pub(crate) fn u8_to_rstr(bytes: Vec<u8>) -> Rstr {
    Rstr::from_string(&unsafe { String::from_utf8_unchecked(bytes) })
}

pub(crate) fn gz_compressed(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map_or(false, |s| s.eq_ignore_ascii_case("gz"))
}

pub(crate) fn gzip_pack(bytes: &[u8], compressor: &mut Compressor) -> Result<Vec<u8>> {
    let pack_size = compressor.gzip_compress_bound(bytes.len());
    let mut pack = Vec::with_capacity(pack_size);
    pack.resize(pack_size, 0);
    let size = compressor.gzip_compress(bytes, &mut pack)?;
    pack.truncate(size);
    Ok(pack)
}

pub(crate) fn new_writer<P: AsRef<Path> + ?Sized>(
    file: &P,
    progress_bar: Option<ProgressBar>,
) -> Result<Box<dyn Write>> {
    let path: &Path = file.as_ref();
    let file = File::create(path)
        .with_context(|| format!("Failed to create output file {}", path.display()))?;
    let writer: Box<dyn Write>;
    if let Some(bar) = progress_bar {
        writer = Box::new(ProgressBarWriter::new(file, bar));
    } else {
        writer = Box::new(file);
    }
    Ok(writer)
}

#[cfg(feature = "isal")]
pub(crate) fn new_reader<P: AsRef<Path> + ?Sized>(
    file: &P,
    buffer_size: usize,
    progress_bar: Option<ProgressBar>,
) -> Result<Box<dyn Read>> {
    let path: &Path = file.as_ref();
    let file =
        File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;
    let reader: Box<dyn Read>;
    if gz_compressed(path) {
        if let Some(bar) = progress_bar {
            reader = Box::new(GzipDecoder::new(BufReader::with_capacity(
                buffer_size,
                ProgressBarReader::new(file, bar),
            )));
        } else {
            reader = Box::new(GzipDecoder::new(BufReader::with_capacity(
                buffer_size,
                file,
            )));
        }
    } else {
        if let Some(bar) = progress_bar {
            reader = Box::new(ProgressBarReader::new(file, bar));
        } else {
            reader = Box::new(file);
        }
    }
    Ok(reader)
}

#[cfg(not(feature = "isal"))]
pub(crate) fn new_reader<P: AsRef<Path> + ?Sized>(
    file: &P,
    buffer_size: usize,
    progress_bar: Option<ProgressBar>,
) -> Result<Box<dyn Read>> {
    let path: &Path = file.as_ref();
    let file =
        File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;
    let reader: Box<dyn Read>;
    if gz_compressed(path) {
        if let Some(bar) = progress_bar {
            reader = Box::new(GzDecoder::new(BufReader::with_capacity(
                buffer_size,
                ProgressBarReader::new(file, bar),
            )));
        } else {
            reader = Box::new(GzDecoder::new(BufReader::with_capacity(buffer_size, file)));
        }
    } else {
        if let Some(bar) = progress_bar {
            reader = Box::new(ProgressBarReader::new(file, bar));
        } else {
            reader = Box::new(file);
        }
    }
    Ok(reader)
}

pub(crate) fn robj_to_option_str(robj: &Robj) -> Result<Option<Vec<&str>>> {
    if robj.is_null() {
        Ok(None)
    } else {
        robj.as_str_vector()
            .map(|s| Some(s))
            .ok_or(anyhow!("must be a character"))
    }
}

pub(crate) fn new_channel<T>(nqueue: Option<usize>) -> (Sender<T>, Receiver<T>) {
    if let Some(queue) = nqueue {
        bounded(queue)
    } else {
        unbounded()
    }
}

pub(crate) fn progress_reader_style() -> std::result::Result<ProgressStyle, TemplateError> {
    ProgressStyle::with_template(
        "{prefix:.bold.cyan/blue} {decimal_bytes}/{decimal_total_bytes} {spinner:.green} [{elapsed_precise}] {decimal_bytes_per_sec} (ETA {eta})",
    )
}

pub(crate) fn progress_writer_style() -> std::result::Result<ProgressStyle, TemplateError> {
    ProgressStyle::with_template(
        "{prefix:.bold.cyan/blue} {decimal_bytes} {spinner:.green} {decimal_bytes_per_sec}",
    )
}
