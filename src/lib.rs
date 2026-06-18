#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]
#![doc(test(
    no_crate_inject,
    attr(deny(warnings, rust_2018_idioms), allow(dead_code, unused_variables))
))]
#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! Provides abstractions for working with bytes.
//!
//! The `bytes` crate provides an efficient byte buffer structure
//! ([`Bytes`]) and traits for working with buffer
//! implementations ([`Buf`], [`BufMut`]).
//!
//! # `Bytes`
//!
//! `Bytes` is an efficient container for storing and operating on contiguous
//! slices of memory. It is intended for use primarily in networking code, but
//! could have applications elsewhere as well.
//!
//! `Bytes` values facilitate zero-copy network programming by allowing multiple
//! `Bytes` objects to point to the same underlying memory. This is managed by
//! using a reference count to track when the memory is no longer needed and can
//! be freed.
//!
//! A `Bytes` handle can be created directly from an existing byte store (such as `&[u8]`
//! or `Vec<u8>`), but usually a `BytesMut` is used first and written to. For
//! example:
//!
//! ```rust
//! use bytes::{BytesMut, BufMut};
//!
//! let mut buf = BytesMut::with_capacity(1024);
//! buf.put(&b"hello world"[..]);
//! buf.put_u16(1234);
//!
//! let a = buf.split();
//! assert_eq!(a, b"hello world\x04\xD2"[..]);
//!
//! buf.put(&b"goodbye world"[..]);
//!
//! let b = buf.split();
//! assert_eq!(b, b"goodbye world"[..]);
//!
//! assert_eq!(buf.capacity(), 998);
//! ```
//!
//! In the above example, only a single buffer of 1024 is allocated. The handles
//! `a` and `b` will share the underlying buffer and maintain indices tracking
//! the view into the buffer represented by the handle.
//!
//! See the [struct docs](`Bytes`) for more details.
//!
//! # `Buf`, `BufMut`
//!
//! These two traits provide read and write access to buffers. The underlying
//! storage may or may not be in contiguous memory. For example, `Bytes` is a
//! buffer that guarantees contiguous memory, but a [rope] stores the bytes in
//! disjoint chunks. `Buf` and `BufMut` maintain cursors tracking the current
//! position in the underlying byte storage. When bytes are read or written, the
//! cursor is advanced.
//!
//! [rope]: https://en.wikipedia.org/wiki/Rope_(data_structure)
//!
//! ## Relation with `Read` and `Write`
//!
//! At first glance, it may seem that `Buf` and `BufMut` overlap in
//! functionality with [`std::io::Read`] and [`std::io::Write`]. However, they
//! serve different purposes. A buffer is the value that is provided as an
//! argument to `Read::read` and `Write::write`. `Read` and `Write` may then
//! perform a syscall, which has the potential of failing. Operations on `Buf`
//! and `BufMut` are infallible.

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

pub mod buf;
pub use crate::buf::{Buf, BufMut};

mod bytes;
mod bytes_mut;
mod fmt;
mod loom;
pub use crate::bytes::Bytes;
pub use crate::bytes_mut::BytesMut;

// Optional Serde support
#[cfg(feature = "serde")]
mod serde;

#[inline(never)]
#[cold]
fn abort() -> ! {
    #[cfg(feature = "std")]
    {
        std::process::abort();
    }

    #[cfg(not(feature = "std"))]
    {
        struct Abort;
        impl Drop for Abort {
            fn drop(&mut self) {
                panic!();
            }
        }
        let _a = Abort;
        panic!("abort");
    }
}

#[inline(always)]
#[cfg(feature = "std")]
fn saturating_sub_usize_u64(a: usize, b: u64) -> usize {
    match usize::try_from(b) {
        Ok(b) => a.saturating_sub(b),
        Err(_) => 0,
    }
}

#[inline(always)]
#[cfg(feature = "std")]
fn min_u64_usize(a: u64, b: usize) -> usize {
    match usize::try_from(a) {
        Ok(a) => usize::min(a, b),
        Err(_) => b,
    }
}

/// Performs bounds checking of a range.
///
/// This is a spiritual copy of [core::slice::index::range] because that
/// function is currently unstable.
#[inline(always)]
#[track_caller]
fn range(range: impl core::ops::RangeBounds<usize>, len: usize) -> (usize, usize) {
    use core::ops::Bound;

    let begin = match range.start_bound() {
        Bound::Included(&n) => n,
        Bound::Excluded(&n) => n.checked_add(1).expect("out of range"),
        Bound::Unbounded => 0,
    };

    let end = match range.end_bound() {
        Bound::Included(&n) => n.checked_add(1).expect("out of range"),
        Bound::Excluded(&n) => n,
        Bound::Unbounded => len,
    };

    assert!(
        begin <= end,
        "range start must not be greater than end: {:?} <= {:?}",
        begin,
        end,
    );
    assert!(
        end <= len,
        "range end out of bounds: {:?} <= {:?}",
        end,
        len,
    );

    (begin, end)
}

/// Error type for the `try_get_` methods of [`Buf`].
/// Indicates that there were not enough remaining
/// bytes in the buffer while attempting
/// to get a value from a [`Buf`] with one
/// of the `try_get_` methods.
#[derive(Debug, PartialEq, Eq)]
pub struct TryGetError {
    /// The number of bytes necessary to get the value
    pub requested: usize,

    /// The number of bytes available in the buffer
    pub available: usize,
}

impl core::fmt::Display for TryGetError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        write!(
            f,
            "Not enough bytes remaining in buffer to read value (requested {} but only {} available)",
            self.requested, self.available
        )
    }
}

#[cfg(feature = "std")]
impl std::error::Error for TryGetError {}

#[cfg(feature = "std")]
impl From<TryGetError> for std::io::Error {
    fn from(error: TryGetError) -> Self {
        std::io::Error::new(std::io::ErrorKind::Other, error)
    }
}

/// Panic with a nice error message.
#[cold]
fn panic_advance(error_info: &TryGetError) -> ! {
    panic!(
        "advance out of bounds: the len is {} but advancing by {}",
        error_info.available, error_info.requested
    );
}

#[cold]
fn panic_does_not_fit(size: usize, nbytes: usize) -> ! {
    panic!(
        "size too large: the integer type can fit {} bytes, but nbytes is {}",
        size, nbytes
    );
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::println;
    use std::vec::Vec;

    use crate::bytes_mut::KIND_VEC;
    use crate::{BufMut, BytesMut};

    const RESET: &str = "\x1b[0m";
    const RED: &str = "\x1b[0;31m";
    const BLUE: &str = "\x1b[0;34m";

    fn print(cache: &BytesMut, title: &str) {
        println!(
            "{title} {:<4?} at 0x{:06x} + {}",
            (cache.len(), cache.capacity()),
            unsafe {
                (if cache.kind() == KIND_VEC {
                    cache.ptr.as_ptr() as usize - cache.get_vec_pos()
                } else {
                    (*cache.data).vec.as_ptr() as usize
                }) & 0xFFFFFF
            },
            unsafe {
                (if cache.kind() == KIND_VEC {
                    cache.get_vec_pos()
                } else {
                    cache.ptr.as_ptr() as usize - (*cache.data).vec.as_ptr() as usize
                }) & 0xFFFFFF
            },
        );
    }

    fn print_split_from_reused(buf: &BytesMut, out: &BytesMut) {
        println!("{BLUE}split from reused:");
        print(out, "    out:        ");
        print(buf, "    remaining:  ");
        println!("{RESET}");
    }

    fn print_new_alloc(buf: &BytesMut, out: &BytesMut) {
        println!("{RED}new alloc:");
        print(out, "    out:        ");
        print(buf, "    remaining:  ");
        println!("{RESET}");
    }

    #[test]
    fn test1() {
        println!();

        let mut pool = Pool::new(10 * 1500, 16);

        let data = [1; 983];

        let mut packets = VecDeque::new();

        for _ in 0..11 {
            packets.push_back(pool.put_packet(&data));
        }

        let _first = packets.pop_front();

        for _ in 0..100 {
            packets.pop_front();
            packets.push_back(pool.put_packet(&data));
        }

        println!();
    }

    #[test]
    fn test2() {
        println!();

        let mut pool = Pool::new(10 * 1500, 16);

        let mut packets = VecDeque::new();

        let reclaim_size = 16 + 983;
        // let reclaim_size = 16 + mtu; // if size is not known

        let read_packet_fn = |buf: &mut BytesMut| {
            let header_size = 100;
            buf.put_bytes(1, header_size);

            let payload_size = 883;
            buf.put_bytes(1, payload_size);

            header_size + payload_size
        };

        for _ in 0..11 {
            packets.push_back(pool.put_packet_with(reclaim_size, read_packet_fn));
        }

        let _first = packets.pop_front();

        for _ in 0..100 {
            packets.pop_front();
            packets.push_back(pool.put_packet_with(reclaim_size, read_packet_fn));
        }

        println!();
    }

    pub struct Pool {
        storage_queue: Vec<BytesMut>,
        storage_cursor: usize,
        storage_capacity: usize,
        reserved_size: usize,
    }

    impl Pool {
        pub fn new(storage_capacity: usize, reserved_size: usize) -> Self {
            Self {
                storage_queue: Vec::new(),
                storage_cursor: 0,
                storage_capacity,
                reserved_size,
            }
        }

        pub fn truncate(&mut self, len: usize) {
            self.storage_queue.truncate(len);
        }

        pub fn put_packet(&mut self, src: &[u8]) -> BytesMut {
            self.put_packet_with(self.reserved_size + src.len(), |buf| {
                buf.put(src);
                src.len()
            })
        }

        // pub async fn put_packet_async_with();

        pub fn put_packet_with(
            &mut self,
            reclaim_size: usize,
            read_packet_fn: impl Fn(&mut BytesMut) -> usize,
        ) -> BytesMut {
            let len = self.storage_queue.len();

            let (head, tail) = self.storage_queue.split_at_mut(self.storage_cursor);

            for (n, buf) in std::iter::chain(tail, head).take(100.min(len)).enumerate() {
                if buf.try_reclaim(reclaim_size) {
                    let out = Self::put_and_split(buf, self.reserved_size, read_packet_fn);
                    print_split_from_reused(buf, &out);
                    self.storage_cursor = (self.storage_cursor + n) % len;
                    return out;
                }
            }

            self.storage_cursor = len;

            let storage = BytesMut::with_capacity(self.storage_capacity);
            let buf = self.storage_queue.push_mut(storage);
            let out = Self::put_and_split(buf, self.reserved_size, read_packet_fn);
            print_new_alloc(buf, &out);
            out
        }

        fn put_and_split(
            buf: &mut BytesMut,
            reserved_size: usize,
            read_packet_fn: impl Fn(&mut BytesMut) -> usize,
        ) -> BytesMut {
            debug_assert!(reserved_size.is_multiple_of(8));
            buf.put_bytes(0, reserved_size);

            let len = reserved_size + read_packet_fn(buf);

            let aligned_len = len.next_multiple_of(8);
            buf.put_bytes(0, aligned_len - len);

            let mut out = buf.split_to(aligned_len);
            out.truncate(len);
            out
        }
    }
}
