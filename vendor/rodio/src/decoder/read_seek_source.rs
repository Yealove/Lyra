use std::io::{Read, Result, Seek, SeekFrom};

use symphonia::core::io::MediaSource;

/// LOCAL PATCH (Lyra): 缓存流长度，让 byte_len 返回真实值。
/// 上游 0.20.1 硬编码返回 None，导致 symphonia FLAC/MP4 demuxer 的 seek
/// （依赖 byte_len 做字节二分）永远 Unseekable。
pub struct ReadSeekSource<T: Read + Seek + Send + Sync> {
    inner: T,
    len: Option<u64>,
}

impl<T: Read + Seek + Send + Sync> ReadSeekSource<T> {
    /// Instantiates a new `ReadSeekSource<T>` by taking ownership and wrapping the provided
    /// `Read + Seek`er.
    pub fn new(mut inner: T) -> Self {
        // 探测长度后回到起始位置，供后续 probe 正常读取
        let len = inner.seek(SeekFrom::End(0)).ok();
        let _ = inner.seek(SeekFrom::Start(0));
        ReadSeekSource { inner, len }
    }
}

impl<T: Read + Seek + Send + Sync> MediaSource for ReadSeekSource<T> {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

impl<T: Read + Seek + Send + Sync> Read for ReadSeekSource<T> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.inner.read(buf)
    }
}

impl<T: Read + Seek + Send + Sync> Seek for ReadSeekSource<T> {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.inner.seek(pos)
    }
}
