//! Primitives for checking an official release. The release transport has no credential inputs, cookies,
//! automatic logon, OS proxy discovery, automatic redirects or connection pool shared with skill requests.

use std::ffi::c_void;
use std::io::{self, Read};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_AUTOLOGON_SECURITY_LEVEL_HIGH, WINHTTP_DISABLE_AUTHENTICATION, WINHTTP_DISABLE_COOKIES, WINHTTP_DISABLE_REDIRECTS,
    WINHTTP_FLAG_ESCAPE_DISABLE, WINHTTP_FLAG_SECURE, WINHTTP_OPTION_AUTOLOGON_POLICY, WINHTTP_OPTION_CLIENT_CERT_CONTEXT, WINHTTP_OPTION_CONNECT_RETRIES,
    WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_OPTION_DISABLE_GLOBAL_POOLING, WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT, WINHTTP_OPTION_RECEIVE_TIMEOUT,
    WINHTTP_QUERY_RAW_HEADERS_CRLF, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData,
    WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};

use super::http::{HttpError, Response, parse_raw_headers};
use super::wide;
use crate::release::{Attempt, ReleaseUrl};

struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void, call: &'static str) -> Result<Self, HttpError> {
        if raw.is_null() { Err(os_error(call)) } else { Ok(Self(raw)) }
    }

    fn set(&self, option: u32, value: u32) -> Result<(), HttpError> {
        if unsafe { WinHttpSetOption(self.0, option, (&raw const value).cast(), 4) } == 0 { Err(os_error("WinHttpSetOption")) } else { Ok(()) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn os_error(call: &'static str) -> HttpError {
    match unsafe { GetLastError() } {
        12002 => HttpError::Timeout,
        12017 => HttpError::Cancelled,
        code => HttpError::Os { call, code },
    }
}

fn response(handle: &Handle) -> Result<Response, HttpError> {
    let mut size = 0;
    unsafe { WinHttpQueryHeaders(handle.0, WINHTTP_QUERY_RAW_HEADERS_CRLF, null(), null_mut(), &mut size, null_mut()) };
    if size == 0 {
        return Err(os_error("WinHttpQueryHeaders"));
    }
    if size > 64 * 1024 {
        return Err(HttpError::TooLarge);
    }
    let mut header = vec![0u16; (size as usize).div_ceil(2)];
    if unsafe { WinHttpQueryHeaders(handle.0, WINHTTP_QUERY_RAW_HEADERS_CRLF, null(), header.as_mut_ptr().cast(), &mut size, null_mut()) } == 0 {
        return Err(os_error("WinHttpQueryHeaders"));
    }
    let raw = String::from_utf16_lossy(&header[..size as usize / 2]);
    let (status, headers) = parse_raw_headers(&raw).ok_or(HttpError::Os { call: "WinHttpQueryHeaders", code: 0 })?;
    Ok(Response { status, headers, error_body: Vec::new() })
}

fn remaining_ms(work: &Attempt<'_>) -> Result<i32, HttpError> {
    let ms = work.remaining()?.as_millis();
    // Zero disables WinHTTP's timeout. Do not start a blocking call when less than one millisecond remains.
    if ms == 0 { Err(HttpError::Timeout) } else { Ok(ms.min(i32::MAX as u128) as i32) }
}

pub(crate) fn get(target: &ReleaseUrl, limit: usize, work: &Attempt<'_>) -> Result<(Response, Vec<u8>), HttpError> {
    let url = target.url();
    debug_assert!(url.secure && url.port == 443);
    work.remaining()?;
    let session =
        Handle::new(unsafe { WinHttpOpen(wide("tiny-skill-sync release").as_ptr(), WINHTTP_ACCESS_TYPE_NO_PROXY, null(), null(), 0) }, "WinHttpOpen")?;
    session.set(WINHTTP_OPTION_DISABLE_GLOBAL_POOLING, 1)?;
    let connection = Handle::new(unsafe { WinHttpConnect(session.0, wide(&url.host).as_ptr(), 443, 0) }, "WinHttpConnect")?;
    // Keep an already encoded, allowlisted CDN path/query intact. No referrer or default client certificate.
    let request = Handle::new(
        unsafe {
            WinHttpOpenRequest(
                connection.0,
                wide("GET").as_ptr(),
                wide(&url.path_and_query).as_ptr(),
                null(),
                null(),
                null(),
                WINHTTP_FLAG_SECURE | WINHTTP_FLAG_ESCAPE_DISABLE,
            )
        },
        "WinHttpOpenRequest",
    )?;
    request.set(WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_DISABLE_REDIRECTS | WINHTTP_DISABLE_AUTHENTICATION | WINHTTP_DISABLE_COOKIES)?;
    request.set(WINHTTP_OPTION_AUTOLOGON_POLICY, WINHTTP_AUTOLOGON_SECURITY_LEVEL_HIGH)?;
    if unsafe { WinHttpSetOption(request.0, WINHTTP_OPTION_CLIENT_CERT_CONTEXT, null(), 0) } == 0 {
        return Err(os_error("WinHttpSetOption"));
    }
    let header = wide(if url.host == "api.github.com" {
        "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\nAccept-Encoding: identity\r\n"
    } else {
        "Accept: application/octet-stream\r\nAccept-Encoding: identity\r\n"
    });
    // Sending can involve name resolution, connect/TLS and the actual send. No automatic retry across addresses.
    request.set(WINHTTP_OPTION_CONNECT_RETRIES, 1)?;
    let ms = remaining_ms(work)? / 4;
    if ms == 0 {
        return Err(HttpError::Timeout);
    }
    if unsafe { WinHttpSetTimeouts(request.0, ms.min(10_000), ms.min(10_000), ms.min(15_000), ms.min(15_000)) } == 0 {
        return Err(os_error("WinHttpSetTimeouts"));
    }
    if unsafe { WinHttpSendRequest(request.0, header.as_ptr(), (header.len() - 1) as u32, null(), 0, 0, 0) } == 0 {
        return Err(os_error("WinHttpSendRequest"));
    }
    let ms = remaining_ms(work)?.min(15_000) as u32;
    request.set(WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT, ms)?;
    request.set(WINHTTP_OPTION_RECEIVE_TIMEOUT, ms)?;
    if unsafe { WinHttpReceiveResponse(request.0, null_mut()) } == 0 {
        return Err(os_error("WinHttpReceiveResponse"));
    }
    let r = response(&request)?;
    let mut body = Vec::new();
    if r.status == 200 {
        if r.header("content-length").and_then(|v| v.parse::<u64>().ok()).is_some_and(|n| n > limit as u64) {
            return Err(HttpError::TooLarge);
        }
        let mut chunk = vec![0u8; 32 * 1024];
        let mut read_timeout = ms;
        loop {
            let ms = remaining_ms(work)?.min(15_000) as u32;
            if ms < read_timeout {
                request.set(WINHTTP_OPTION_RECEIVE_TIMEOUT, ms)?;
                read_timeout = ms;
            }
            let mut n = 0;
            if unsafe { WinHttpReadData(request.0, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut n) } == 0 {
                return Err(os_error("WinHttpReadData"));
            }
            if n == 0 {
                break;
            }
            if n as usize > limit - body.len() {
                return Err(HttpError::TooLarge);
            }
            body.extend_from_slice(&chunk[..n as usize]);
        }
    }
    work.remaining()?;
    Ok((r, body))
}

const INITIAL: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];

/// One-shot SHA-256, FIPS 180-4. No fallible provider setup and no copy of the input.
pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut state = INITIAL;
    let mut blocks = bytes.chunks_exact(64);
    for block in &mut blocks {
        compress(&mut state, block);
    }
    finish(state, blocks.remainder(), bytes.len() as u64)
}

pub(crate) fn sha256_reader(mut reader: impl Read) -> io::Result<[u8; 32]> {
    let mut state = INITIAL;
    let mut buffer = [0; 16 * 1024];
    let mut filled = 0;
    let mut total = 0u64;
    loop {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => return Ok(finish(state, &buffer[..filled], total)),
            Ok(n) => {
                total = total.wrapping_add(n as u64);
                filled += n;
                let complete = filled / 64 * 64;
                for block in buffer[..complete].chunks_exact(64) {
                    compress(&mut state, block);
                }
                buffer.copy_within(complete..filled, 0);
                filled -= complete;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn finish(mut state: [u32; 8], rest: &[u8], len: u64) -> [u8; 32] {
    let mut padding = [0; 128];
    let end = if rest.len() < 56 { 64 } else { 128 };
    padding[..rest.len()].copy_from_slice(rest);
    padding[rest.len()] = 0x80;
    padding[end - 8..end].copy_from_slice(&len.wrapping_mul(8).to_be_bytes());
    for block in padding[..end].chunks_exact(64) {
        compress(&mut state, block);
    }
    let mut out = [0; 32];
    for (chunk, word) in out.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn compress(state: &mut [u32; 8], block: &[u8]) {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut w = [0u32; 64];
    for (word, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes(chunk.try_into().expect("4 bytes"));
    }
    for i in 16..64 {
        let a = w[i - 15];
        let b = w[i - 2];
        let s0 = a.rotate_right(7) ^ a.rotate_right(18) ^ (a >> 3);
        let s1 = b.rotate_right(17) ^ b.rotate_right(19) ^ (b >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for (k, word) in K.into_iter().zip(w) {
        let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ (!e & g);
        let t1 = h.wrapping_add(sum1).wrapping_add(choose).wrapping_add(k).wrapping_add(word);
        let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let t2 = sum0.wrapping_add(majority);
        [a, b, c, d, e, f, g, h] = [t1.wrapping_add(t2), a, b, c, d.wrapping_add(t1), e, f, g];
    }
    for (slot, word) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(word);
    }
}
