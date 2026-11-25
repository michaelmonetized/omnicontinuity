use crate::{MAX_FILES, Result, SendFile, protocol};
use flate2::{
    Compression,
    bufread::{GzDecoder, ZlibDecoder},
    write::ZlibEncoder,
};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    fs::File,
    io::{self, BufReader, Read, Seek, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

const BLOCK: usize = 65536;

pub(crate) struct DvzipWriter<W> {
    output: W,
    block: Vec<u8>,
}
impl<W: Write> DvzipWriter<W> {
    pub fn new(output: W) -> Self {
        Self {
            output,
            block: Vec::with_capacity(BLOCK),
        }
    }
    fn emit(&mut self) -> io::Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(6));
        encoder.write_all(&self.block)?;
        let compressed = encoder.finish()?;
        if compressed.len() < self.block.len() {
            self.output
                .write_all(&(compressed.len() as u32).to_be_bytes())?;
            self.output.write_all(&compressed)?;
        } else {
            self.output
                .write_all(&(0x80000000 | self.block.len() as u32).to_be_bytes())?;
            self.output.write_all(&self.block)?;
        }
        self.block.clear();
        Ok(())
    }
    pub fn finish(mut self) -> Result<W> {
        self.emit()?;
        self.output.flush()?;
        Ok(self.output)
    }
}
impl<W: Write> Write for DvzipWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(BLOCK - self.block.len());
        self.block.extend_from_slice(&bytes[..count]);
        if self.block.len() == BLOCK {
            self.emit()?;
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

pub(crate) fn decode_dvzip(mut input: File, output: &mut File, limit: u64) -> Result<()> {
    let mut magic = [0; 2];
    let read = input.read(&mut magic)?;
    input.rewind()?;
    if read == 2 && magic == [0x1f, 0x8b] {
        let mut decoder = GzDecoder::new(BufReader::new(input));
        bounded_copy(&mut decoder, output, limit)?;
        let mut tail = [0];
        if decoder.into_inner().read(&mut tail)? != 0 {
            return Err(protocol("gzip archive has trailing data"));
        }
        return Ok(());
    }
    let mut total = 0u64;
    let mut blocks = 0;
    loop {
        let mut header = [0; 4];
        if input.read(&mut header[..1])? == 0 {
            break;
        }
        input.read_exact(&mut header[1..])?;
        let value = u32::from_be_bytes(header);
        let size = u64::from(value & 0x7fffffff);
        blocks += 1;
        if size == 0 || size > limit || blocks > 1_000_000 {
            return Err(protocol("invalid DVZip block size or count"));
        }
        let count = if value & 0x80000000 != 0 {
            let count = bounded_copy(
                &mut (&mut input).take(size),
                output,
                limit.saturating_sub(total),
            )?;
            if count != size {
                return Err(protocol("truncated DVZip stored block"));
            }
            count
        } else {
            let limited = (&mut input).take(size);
            let mut decoder = ZlibDecoder::new(BufReader::new(limited));
            let count = bounded_copy(&mut decoder, output, limit.saturating_sub(total))?;
            let remaining = decoder.into_inner();
            if !remaining.buffer().is_empty() || remaining.get_ref().limit() != 0 {
                return Err(protocol("truncated or trailing DVZip compressed block"));
            }
            count
        };
        total = total
            .checked_add(count)
            .filter(|&n| n <= limit)
            .ok_or_else(|| protocol("decompressed archive exceeds limit"))?;
    }
    if total == 0 {
        return Err(protocol("empty DVZip archive"));
    }
    Ok(())
}

fn bounded_copy(input: &mut impl Read, output: &mut impl Write, limit: u64) -> Result<u64> {
    let mut bytes = [0; BLOCK];
    let mut total = 0u64;
    loop {
        let count = input.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .filter(|&n| n <= limit)
            .ok_or_else(|| protocol("decompressed archive exceeds limit"))?;
        output.write_all(&bytes[..count])?;
    }
    Ok(total)
}

fn odc_header(output: &mut impl Write, name: &str, size: u64, inode: usize) -> Result<()> {
    if size > 0o77777777777 || name.len() + 1 > 0o777777 {
        return Err(protocol("file is too large for odc CPIO"));
    }
    write!(
        output,
        "070707{dev:06o}{inode:06o}{mode:06o}{uid:06o}{gid:06o}{links:06o}{rdev:06o}{mtime:011o}{names:06o}{size:011o}",
        dev = 0,
        mode = 0o100644,
        uid = 0,
        gid = 0,
        links = 1,
        rdev = 0,
        mtime = 0,
        names = name.len() + 1
    )?;
    output.write_all(name.as_bytes())?;
    output.write_all(&[0])?;
    Ok(())
}

pub(crate) fn write_cpio(output: &mut impl Write, files: Vec<SendFile>) -> Result<()> {
    for (index, mut file) in files.into_iter().enumerate() {
        odc_header(output, &format!("./{}", file.name), file.size, index + 1)?;
        crate::http::copy_exact(&mut file.file, output, file.size)?;
        if file.file.metadata()?.len() != file.size {
            return Err(protocol("file changed size during transfer"));
        }
    }
    odc_header(output, "TRAILER!!!", 0, 0)?;
    Ok(())
}

#[link(name = "archive")]
unsafe extern "C" {
    fn archive_read_new() -> *mut c_void;
    fn archive_read_support_format_cpio(archive: *mut c_void) -> c_int;
    fn archive_read_open_fd(archive: *mut c_void, fd: c_int, block_size: usize) -> c_int;
    fn archive_read_next_header(archive: *mut c_void, entry: *mut *mut c_void) -> c_int;
    fn archive_read_data(archive: *mut c_void, buffer: *mut c_void, size: usize) -> isize;
    fn archive_read_free(archive: *mut c_void) -> c_int;
    fn archive_error_string(archive: *mut c_void) -> *const c_char;
    fn archive_entry_pathname(entry: *mut c_void) -> *const c_char;
    fn archive_entry_size(entry: *mut c_void) -> i64;
    fn archive_entry_filetype(entry: *mut c_void) -> u32;
    fn archive_entry_hardlink(entry: *mut c_void) -> *const c_char;
}

struct Archive(*mut c_void);
impl Drop for Archive {
    fn drop(&mut self) {
        unsafe {
            archive_read_free(self.0);
        }
    }
}
impl Archive {
    fn error(&self) -> crate::Error {
        let pointer = unsafe { archive_error_string(self.0) };
        if pointer.is_null() {
            protocol("invalid CPIO archive")
        } else {
            protocol(unsafe { CStr::from_ptr(pointer) }.to_string_lossy())
        }
    }
    fn check(&self, status: c_int) -> Result<()> {
        if status == 0 {
            Ok(())
        } else {
            Err(self.error())
        }
    }
}

pub(crate) fn extract(
    file: &File,
    directory: &Path,
    staging: &Path,
    limit: u64,
) -> Result<Vec<PathBuf>> {
    let archive = Archive(unsafe { archive_read_new() });
    if archive.0.is_null() {
        return Err(protocol("libarchive allocation failed"));
    }
    archive.check(unsafe { archive_read_support_format_cpio(archive.0) })?;
    archive.check(unsafe { archive_read_open_fd(archive.0, file.as_raw_fd(), BLOCK) })?;
    let mut staged = Vec::new();
    let mut total = 0u64;
    let mut members = 0;
    loop {
        let mut entry = std::ptr::null_mut();
        let status = unsafe { archive_read_next_header(archive.0, &mut entry) };
        if status == 1 {
            break;
        }
        archive.check(status)?;
        if entry.is_null() {
            return Err(protocol("missing CPIO entry"));
        }
        members += 1;
        if members > MAX_FILES {
            return Err(protocol("too many archive members"));
        }
        let kind = unsafe { archive_entry_filetype(entry) };
        if kind == libc::S_IFDIR {
            continue;
        }
        if kind != libc::S_IFREG || !unsafe { archive_entry_hardlink(entry) }.is_null() {
            return Err(protocol("archive contains links or special files"));
        }
        let pointer = unsafe { archive_entry_pathname(entry) };
        if pointer.is_null() {
            return Err(protocol("archive file has no name"));
        }
        let name = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .map_err(|_| protocol("archive name is not UTF-8"))?;
        let name = safe_basename(name)?;
        let size = unsafe { archive_entry_size(entry) };
        if size < 0 {
            return Err(protocol("negative archive file size"));
        }
        total = total
            .checked_add(size as u64)
            .filter(|&n| n <= limit)
            .ok_or_else(|| protocol("archive output exceeds limit"))?;
        let mut output = tempfile::NamedTempFile::new_in(staging)?;
        let mut written = 0u64;
        let mut buffer = [0u8; BLOCK];
        loop {
            let count =
                unsafe { archive_read_data(archive.0, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count < 0 {
                return Err(archive.error());
            }
            if count == 0 {
                break;
            }
            written += count as u64;
            if written > size as u64 {
                return Err(protocol("archive entry exceeds its declared size"));
            }
            output.write_all(&buffer[..count as usize])?;
        }
        if written != size as u64 {
            return Err(protocol("truncated archive entry"));
        }
        if !name.starts_with("._") {
            staged.push((name.to_owned(), output));
        }
    }
    if staged.is_empty() {
        return Err(protocol("archive contains no regular files"));
    }
    let mut published = Vec::new();
    let result = (|| {
        for (name, output) in staged {
            output.as_file().sync_all()?;
            let path = publish(output, directory, &name)?;
            published.push(path);
        }
        Ok(published.clone())
    })();
    if result.is_err() {
        for path in &published {
            let _ = std::fs::remove_file(path);
        }
    }
    result
}

fn safe_basename(name: &str) -> Result<&str> {
    if name.starts_with('/') || name.contains('\\') || name.split('/').any(|part| part == "..") {
        return Err(protocol("unsafe archive path"));
    }
    let name = name.rsplit('/').next().unwrap_or("");
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.chars().any(char::is_control)
    {
        return Err(protocol("unsafe archive file name"));
    }
    Ok(name)
}

fn publish(mut file: tempfile::NamedTempFile, directory: &Path, name: &str) -> Result<PathBuf> {
    let original = Path::new(name);
    let stem = original
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("file");
    let extension = original
        .extension()
        .and_then(|v| v.to_str())
        .map(|v| format!(".{v}"))
        .unwrap_or_default();
    for index in 0..10000 {
        let candidate = if index == 0 {
            name.to_owned()
        } else {
            format!("{stem}-{index}{extension}")
        };
        let path = directory.join(candidate);
        match file.persist_noclobber(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => file = e.file,
            Err(e) => return Err(e.error.into()),
        }
    }
    Err(protocol("too many receive file name collisions"))
}
