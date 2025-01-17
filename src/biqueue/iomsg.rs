// Copyright 2020 Steven Bosnick
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE-2.0 or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms

//! Types to provide a safe interface around `libc::recvmsg` and `libc::sendmsg`.

use std::{
    convert::TryInto,
    error, fmt,
    io::{self, IoSlice, IoSliceMut},
    marker::PhantomData,
    mem,
    ops::Neg,
    os::unix::io::{IntoRawFd, RawFd},
    ptr::{self, NonNull},
    slice,
};

// needed until the MSRV is 1.43 when the associated constant becomes available
use std::isize;

use libc::{
    c_int, c_uint, close, cmsghdr, iovec, msghdr, recvmsg, sendmsg, CMSG_DATA, CMSG_FIRSTHDR,
    CMSG_LEN, CMSG_NXTHDR, CMSG_SPACE, MSG_CTRUNC, SCM_RIGHTS, SOL_SOCKET,
};
use num_traits::One;

#[derive(Debug)]
/// The core type providing a safe interface to `libc::recvmsg` and `libc::sendmsg`.
pub struct MsgHdr<'a, State> {
    // Invariant: mhdr is properly initalized with msg_name null, and with
    // msg_iov and msg_control valid pointers for length msg_iovlen and
    // msg_controllen respectively. The arrays that msg_iov and msg_control
    // point to must outlive mhdr. The pointer for msg_control may instead
    // be null (with a 0 msg_controllen) for a State that is a NullableControl
    // state.
    mhdr: msghdr,
    state: State,
    _phantom: PhantomData<(&'a mut [iovec], &'a mut [u8])>,
}

trait NullableControl {}

// The type states for MsgHdr are used to implement the following
// finite state machine:
//
//    -> RecvStart -(recvmsg)-> RecvEnd
//  /
// O
//  \
//    -> SendStart -> SendReady -(sendmsg)-> SendEnd
//
// Most of the states are implemented through a specialization of MsgHdr. The
// exception is for "RecvEnd" which is implemented as its own type MsgHdrRecvEnd.
// This is done to allow it to have a Drop implementation.
//
// The diagram above shows which transitions involve a call to recvmsg or
// sendmsg.
//
// The SendReady state logically holds a (possibility empty) slice of shared
// references to file descriptors. It is actually implemented as an encoding of
// RawFd's into a buffer. This module does not attempt to use the rust type
// system to prevent a caller from closing a RawFd that is encoded into the
// SendReady state, but this would almost certainly lead to incorrect results.
//
// The RecvEnd state logically owns a (possibility empty) slice of file
// descriptors until the calling code takes them by iterating the Iterator
// returned by take_fds(). If the RecvEnd state or the Iterator returned
// by take_fds() is dropped before all of the owned file descriptors have
// been taken then the remaining file descriptors are closed.

#[derive(Debug, Default)]
pub struct RecvStart {}

// This is loggically a MsgHdr<'a, RecvEnd> but we need to implement
// Drop for it and Drop impls can't be specialized (rustc error E0366).
//
// This type owns the file descriptors encoded into mhdr.msg_control until they
// are taken (by a call to take_fds()) after which they are owned a single
// FdsIter.
//
// This state occurs after a call to recvmsg() which means that the file
// descriptors in msg_control are new to the current process. Until they
// are taken (by iterating the return value of take_fds()) the only
// part of the process that knows about them is MsgHdrRecvEnd and
// FdsIter. The Drop implementations on those two types are
// responsible for closing any file descriptors that haven't been
// taken. Any file descriptors that have been taken are the caller's
// responsibility.
#[derive(Debug)]
pub struct MsgHdrRecvEnd<'a> {
    // Invariant: the same as MsgHdr for a non-NullableControl State
    mhdr: msghdr,
    bytes_recvieved: usize,
    fds_taken: bool,
    _phantom: PhantomData<(&'a mut [iovec], &'a mut [u8])>,
}

#[derive(Debug, Default)]
pub struct SendStart {}

#[derive(Debug)]
pub struct SendReady {
    fds_count: usize,
}

impl NullableControl for SendReady {}

#[derive(Debug)]
pub struct SendEnd {
    bytes_sent: usize,
    fds_sent: usize,
}

impl NullableControl for SendEnd {}

struct FdsIter<'a> {
    // Invariant: mhdr is initalized as described in MsgHdr that has
    // been filled in by a call to recvmsg.
    mhdr: &'a msghdr,
    // Invariant: cmsg is a valid cmsg based on mhdr (or None)
    cmsg: Option<&'a cmsghdr>,
    data: Option<FdsIterData>,
}

// Invariants:
//      1. curr and end are non-null
//      2. curr <= end
//      3. the half-open range [curr, end) (if non-empty) must
//          be all part of the same same array of initalized
//          RawFd values.
//      4. if curr != end then curr must be a valid pointer
//  Note that neither curr nor end is assumed to be aligned.
struct FdsIterData {
    curr: *const RawFd,
    end: *const RawFd,
}

struct CMsgMut<'a> {
    // Invariant: cmsg is a properly aligned, dereferenceable cmsghdr
    // that has been initalized. It must also not be possible to
    // get a different pointer to the memory pointed to by cmsg. Finally,
    // the bytes buffer pointed to by cmsg must be valid for
    // reads and writes for cmsg.cmsg_len bytes and the bytes in this
    // range after the cmsghdr must be initalized.
    cmsg: NonNull<cmsghdr>,
    _phantom: PhantomData<&'a mut msghdr>,
}

/// A safe owner of a contained RawFd.
#[derive(Debug)]
pub struct Fd {
    // Invariant: fd is None or Fd is the owner of the contained RawFd.
    fd: Option<RawFd>,
}

impl<'a, State: Default> MsgHdr<'a, State> {
    // Safety: iov must be valid for length iov_len and the array that iov points to
    // must outlive the returned MsgHdr.
    unsafe fn new(iov: *mut iovec, iov_len: usize, cmsg_buffer: &'a mut [u8]) -> Self {
        let mhdr = {
            // msghdr may have private members for padding. This ensures that they are zeroed.
            let mut mhdr = mem::MaybeUninit::<libc::msghdr>::zeroed();
            // Safety: we don't turn p into a reference or read from it
            let p = mhdr.as_mut_ptr();
            (*p).msg_name = ptr::null_mut();
            (*p).msg_namelen = 0;
            (*p).msg_iov = iov;
            (*p).msg_iovlen = iov_len;
            (*p).msg_control = cmsg_buffer.as_mut_ptr() as _;
            (*p).msg_controllen = cmsg_buffer.len();
            (*p).msg_flags = 0;
            // Safety: we have initalized all 7 fields of mhdr and zeroed the padding
            mhdr.assume_init()
        };

        // Invariant: msg_name is set to null; msg_control (and its len) are
        // set based on a slice that outlives mhdr; the requirements on
        // msg_iov (and its len) are precondition to this function.
        Self {
            mhdr,
            state: Default::default(),
            _phantom: PhantomData,
        }
    }
}

impl<'a> MsgHdr<'a, RecvStart> {
    pub fn from_io_slice_mut(bufs: &'a mut [IoSliceMut], cmsg_buffer: &'a mut [u8]) -> Self {
        // IoSliceMut guarentees ABI compatibility with iovec.
        let iov: *mut iovec = bufs.as_mut_ptr() as *mut iovec;
        let iov_len = bufs.len();

        // Safety: iov is valid for iov_len because they both come from the same
        // slice (bufs). The array that iov points to will outlive the returned
        // MsgHdr because of the lifetime constraints on bufs and on MsgHdr.
        unsafe { Self::new(iov, iov_len, cmsg_buffer) }
    }

    pub fn recv(mut self, sockfd: RawFd) -> io::Result<MsgHdrRecvEnd<'a>> {
        // Safety: the invariants on self.mhdr mean that it has been properly
        // initalized for passing to recvmsg.
        let count =
            call_res(|| unsafe { recvmsg(sockfd, &mut self.mhdr, 0) }).map(|c| c as usize)?;

        // Invariant: self.mhdr satified the invariant at the start of this call.
        // recvmsg can write into the buffers pointed to by the iovec's found
        // in the array pointed to by mhdr.iov but won't change the array itself.
        // recvmsg may change msg_controllen to the length of the actual control
        // buffer read, but this will be no longer than the msg_controllen passed
        // in so msg_control will still be a valid pointer for length
        // msg_controllen.
        Ok(MsgHdrRecvEnd {
            mhdr: self.mhdr,
            bytes_recvieved: count,
            fds_taken: false,
            _phantom: PhantomData,
        })
    }
}

impl<'a> MsgHdrRecvEnd<'a> {
    pub fn bytes_recvieved(&self) -> usize {
        self.bytes_recvieved
    }

    pub fn was_control_truncated(&self) -> bool {
        self.mhdr.msg_flags & MSG_CTRUNC != 0
    }

    pub fn take_fds<'b>(&'b mut self) -> impl Iterator<Item = Fd> + 'b {
        if self.fds_taken {
            FdsIter::empty(&self.mhdr)
        } else {
            self.fds_taken = true;
            // Safety: the invariant on self.mhdr means it is initalized
            // appropriately. The trasition from RecvStart state to RecvEnd
            // state means that recvmsg was called. The appropriate lifetimes
            // for the buffers in mhdr are guarenteed by holding a shared
            // reference to self and by the absence of other methods that
            // can modify a MsgHdr<RecvEnd>.
            unsafe { FdsIter::new(&self.mhdr) }
        }
    }
}

// Close the file descriptors in the MsgHdrRecvEnd unless they have been taken
// by a call to take_fds(), in which case the returned FdsIter is responsible
// for closing them.
//
// The Drop implementations on MsgHdrRecvEnd and FdsIter use a leak amplification
// stategy to avoid calling libc::close() twice on the same file descriptor.
// See the discussion in the Nomicon at
// https://doc.rust-lang.org/nomicon/leaking.html#drain for a general description
// of this issue.
//
// Specifally the following code will leak open file descriptors but will not
// attempt to call libc::close() twice on the same file descriptor:
//
//      let mut r: MsgHdrRecvEnd = ...;
//      mem::forget(r.take_fds());
//      mem::drop(r);
impl<'a> Drop for MsgHdrRecvEnd<'a> {
    fn drop(&mut self) {
        if !self.fds_taken {
            drop(self.take_fds());
        }
    }
}

impl<'a> MsgHdr<'a, SendStart> {
    pub fn from_io_slice(bufs: &'a [IoSlice], cmsg_buffer: &'a mut [u8]) -> Self {
        // IoSlice guarentees ABI compatibility with iovec. sendmsg doesn't
        // mutate the iovec array but the standard says it takes a mutable
        // pointer so this transmutes he pointer into a mutable one. The State
        // constraint of SendStart on MsgHdr will prevent calling recvmsg on the
        // underlying msghdr.
        let iov: *mut iovec = bufs.as_ptr() as *mut iovec;
        let iov_len = bufs.len();

        // Safety: iov is valid for iov_len because they both come from the same
        // slice (bufs). The array that iov points to will outlive the returned
        // MsgHdr because of the lifetime constraints on bufs and on MsgHdr.
        unsafe { Self::new(iov, iov_len, cmsg_buffer) }
    }

    /// The caller is responsible for ensuring that all of the file descriptors
    /// from the `fds` iterator remain open until after the call to `send()`.
    pub fn encode_fds(
        mut self,
        fds: impl Iterator<Item = RawFd>,
    ) -> io::Result<MsgHdr<'a, SendReady>> {
        // Safety: the invariants on self.mhdr satify the preconditions of first_cmsg.
        let count = match unsafe { CMsgMut::first_cmsg(&mut self.mhdr, SOL_SOCKET, SCM_RIGHTS) } {
            None => {
                if fds.count() > 0 {
                    return Err(CMsgBufferTooSmallError::new());
                }
                0
            }
            Some(mut cmsg) => {
                let mut count = 0;
                let mut data = cmsg.data();

                for fd in fds {
                    let fd_size = mem::size_of_val(&fd);
                    if data.len() < fd_size {
                        return Err(CMsgBufferTooSmallError::new());
                    }

                    let (nextval, nextdata) = data.split_at_mut(fd_size);
                    nextval.copy_from_slice(&fd.to_ne_bytes());

                    data = nextdata;
                    count += 1;
                }
                cmsg.shrink_data_len((count * mem::size_of::<RawFd>()).try_into().unwrap());
                count
            }
        };

        // Adjust msg_control* now that we know the count of the fds
        if count == 0 {
            self.mhdr.msg_control = ptr::null_mut();
            self.mhdr.msg_controllen = 0;
        } else {
            self.mhdr.msg_controllen = cmsg_buffer_fds_space(count);
        }

        // Invariant: self.mhdr satified the invariant at the start of the method.
        // If count is non-zero then msg_controllen may be shortened but this
        // still satifies the invariant (it is not lengthend because of the
        // "curr >= end" guard in the loop). If count is 0 then msg_control
        // is set to null (with a 0 msg_controllen) but this is allowed since
        // the next State (SendReady) is a NullableControl state.
        Ok(MsgHdr {
            mhdr: self.mhdr,
            state: SendReady { fds_count: count },
            _phantom: PhantomData,
        })
    }
}

impl<'a> MsgHdr<'a, SendReady> {
    pub fn send(self, sock_fd: RawFd) -> io::Result<MsgHdr<'a, SendEnd>> {
        // Safety: the invariants on self.mhdr mean that it has been properly
        // initalized for passing to sendmsg.
        let bytes_sent =
            call_res(|| unsafe { sendmsg(sock_fd, &self.mhdr, 0) }).map(|c| c as usize)?;

        // Invariant: self.mhdr satified the invariants at the start of this
        // call and sendmsg does not change it. SendEnd (like SendReady) is
        // a NullableControl state so that part of the invariant also carries
        // through.
        Ok(MsgHdr {
            mhdr: self.mhdr,
            state: SendEnd {
                bytes_sent,
                fds_sent: self.state.fds_count,
            },
            _phantom: PhantomData,
        })
    }
}

impl<'a> MsgHdr<'a, SendEnd> {
    pub fn bytes_sent(&self) -> usize {
        self.state.bytes_sent
    }

    pub fn fds_sent(&self) -> usize {
        self.state.fds_sent
    }
}

impl<'a> FdsIter<'a> {
    // Safety: mhdr is initalized as described in invariant for MsgHdr and
    // has been filled in by a call to recvmsg. The buffers pointed to
    // by fields of mhdr must not be changed during the lifetime of the
    // returned reference.
    unsafe fn new(mhdr: &'a msghdr) -> Self {
        // Safety: follows from pre-condition.
        let cmsg = FdsIter::first_cmsg(mhdr);
        // Safety: follows from pre-condition (especially the call to recvmsg).
        let data = cmsg.and_then(|cmsg| FdsIterData::new(cmsg));

        FdsIter {
            // Invariant: follows from pre-condition.
            mhdr,
            // Invariant: cmsg is produced by first_cmsg based on mhdr
            cmsg,
            data,
        }
    }

    fn empty(mhdr: &'a msghdr) -> Self {
        FdsIter {
            mhdr,
            cmsg: None,
            data: None,
        }
    }

    // Safety: mhdr is initalized as described in invariant for MsgHdr and
    // has been filled in by a call to recvmsg. The buffers pointed to
    // by fields of mhdr must not be changed during the lifetime of the
    // returned reference.
    unsafe fn first_cmsg(mhdr: &'a msghdr) -> Option<&'a cmsghdr> {
        // Safety: follows from pre-condition.
        let cmsg = CMSG_FIRSTHDR(mhdr);
        // Safety: if CMSG_FIRSTHDR returns a non-null pointer it will be a
        // properly aligned pointer to a cmsghdr. It also guarentees that it
        // points to a part of the msg_control that is big enough for a cmsghdr
        // which means that the pointer is dereferenceable. Finally, the
        // precondition on not changing the buffers of the fields pointed to by
        // mhdr and the lifetime constraint on the functions return value
        // enforce Rust's aliasing rules.
        cmsg.as_ref()
    }

    fn advance_cmsg(&mut self) {
        if let Some(cmsg) = self.cmsg {
            // Safety: cmsg is a valid cmsg based on mhdr which has a
            // msg_control array filled in by recvmsg (from the invariants).
            // The call to CMSG_NXTHDR is thus safe by its defintion. If
            // CMSG_NXTHDR returns a non-null pointer it will be a properly
            // alligned pointer to a cmsghdr. It also points to a part of the
            // msg_control that is big enough for a cmsghdr which means that
            // the pointer is dereferenceable. Finally, the precondition on not
            // changing the buffers of the fields pointed to by mhdr and the
            // lifetime constraint on the functions return value enforce Rust's
            // aliasing rules.
            let new_cmsg = unsafe { CMSG_NXTHDR(self.mhdr, cmsg).as_ref() };

            // Safety: follows from the invariant on msghdr and especially
            // the requirement to have called recvmsg.
            let new_data = new_cmsg.and_then(|cmsg| unsafe { FdsIterData::new(cmsg) });

            // Invariant: new_cmsg is produced by a call to CMSG_NXTHDR.
            self.cmsg = new_cmsg;
            self.data = new_data;
        }
    }
}

impl<'a> Iterator for FdsIter<'a> {
    type Item = Fd;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.data.as_mut().and_then(|data| data.next()) {
                Some(data) => return Some(data),
                None => {
                    self.advance_cmsg();
                    if self.cmsg.is_none() {
                        return None;
                    }
                }
            };
        }
    }
}

// See the comment on Drop for MsgHdrRecvEnd for the general Drop strategy.
// This Drop impl will close all remaining file descriptors that are
// (logically) owned by this FdsIter. There can be at most one non-empty
// FdsIter for a given MsgHdrRecvEnd. If it is dropped then any remaining
// file descriptors that haven't been iterated never will and there will
// be not other references to those file descriptors in the process so
// we close them here to avoid leaking them.
impl<'a> Drop for FdsIter<'a> {
    fn drop(&mut self) {
        for fd in self {
            drop(fd);
        }
    }
}

impl FdsIterData {
    // Safety: cmsg must be properly initalized for cmsg.cmsg_len bytes
    // starting at cmsg. That is, the implict cmsg_data member of cmsg must
    // be properly initalized.
    unsafe fn new(cmsg: &cmsghdr) -> Option<Self> {
        if cmsg.cmsg_level == SOL_SOCKET && cmsg.cmsg_type == SCM_RIGHTS {
            // Safety: follows from pre-condition
            let p_start = CMSG_DATA(cmsg) as *const u8;

            assert!(cmsg.cmsg_len <= (isize::MAX as usize));
            let pcmsg: *const cmsghdr = cmsg;
            // Safety: follows from pre-condition,  from the defintion of a
            // cmsg, and from the assertion above.
            let p_end = (pcmsg.cast::<u8>()).offset(cmsg.cmsg_len as isize);

            let data_size = (p_end as usize) - (p_start as usize);
            // This may round down if the data portion is bigger than an
            // integral number of RawFd's.
            let fds_count: usize = data_size / mem::size_of::<RawFd>();

            let curr = p_start.cast::<RawFd>();
            // Safety: curr points to the first byte of the implict cmsg_data
            // member of the cmsg which is properly initalized by the precondition;
            // curr.offset(fds_count) is <= p_end by the way fds_count is calculated
            // so it is also either in the implict cmsg_data member of cmsg or is
            // one byte past the end; the assertion above guarentees that the offset
            // in bytes implied by fds_count <= isize::MAX.
            let end = curr.offset(fds_count as isize);

            // Invariants:
            //      1. curr is non-null by defintion of CMSG_DATA; end is
            //          non-null by defintion of offset.
            //      2. end is offset from curr by fds_count which is non-negative
            //          so curr <= end.
            //      3. cmsg is properly initalized (by the precondition) and is
            //          of type SCM_RIGHTS which means that its data portion
            //          is an array of RawFd's; the calculations of curr and
            //          end above give the first and one-past-the-last elements
            //          of this array so [curr, end) is the whole array of
            //          RawFd's that make up the the data portion of cmsg.
            //      4. Follows from 3.
            Some(FdsIterData { curr, end })
        } else {
            None
        }
    }
}

impl Iterator for FdsIterData {
    type Item = Fd;

    fn next(&mut self) -> Option<Self::Item> {
        if self.curr < self.end {
            // Safety: curr < end so follows from invariants 1, 3 and 4.
            let next = unsafe { self.curr.read_unaligned() };

            // Safety: curr < end so curr is valid by invariant 4;
            // curr.offset(1) is either end or and element of
            // [curr, end) by invariant 3 and is thus either one
            // past the end of the RawFd array or in bounds for the
            // RawFd array. The offset in bytes is sizeof::<RawFd>()
            // which is (much) less than isize::MAX.
            //
            // Invariants: invariant 1 is maintained by defintion of offset();
            // invariant 3 (from entry to method) and curr < end means that
            // curr.offset(1) <= end so invariants 2 and 3 are maintained;
            // invariant 3 being maintained imples that invariant 4 is maintained.
            self.curr = unsafe { self.curr.offset(1) };

            // Precondtion: FdsIterData is the logical owner of all of the remaining
            // file descriptors in the current cmsg. We have copied the next
            // RawFd out of the logical slice of RawFd's and then advanced the curr
            // pointer past that location. `next` is thus the only logical copy of
            // that RawFd.
            Some(Fd::new(next))
        } else {
            None
        }
    }
}

impl<'a> CMsgMut<'a> {
    // Safety: mhdr.msg_control must be null or point to an initalized byte
    // buffer of length mhdr.msg_controllen that lives at least as long as mhdr.
    // The byte buffer starting at msg_control must be part of a single allocated
    // object.
    unsafe fn first_cmsg(mhdr: &'a mut msghdr, level: c_int, typ: c_int) -> Option<Self> {
        // Safety: the precondition on mhdr ensures that it is safe to call
        // CMSG_FIRSTHDR.
        let cmsg = CMSG_FIRSTHDR(mhdr);

        if cmsg == ptr::null_mut() {
            None
        } else {
            // Safety: from the precondition msg_control points to a byte
            // buffer of length msg_controllen so control_max is one past the
            // end of this buffer. The "try_into().unwrap()" ensures that
            // msg_controllen does not overflow an isize (and the offset is
            // already in bytes).
            let control_max =
                (mhdr.msg_control.cast::<u8>()).offset(mhdr.msg_controllen.try_into().unwrap());
            // Safety: cmsg is a non-null return from CMSG_FIRSTHDR.
            let data = CMSG_DATA(cmsg);
            let data_size = (control_max as usize) - (data as usize);
            // Safety: CMSG_LEN is safe.
            let max_len = CMSG_LEN(data_size.try_into().unwrap());

            // Safety: cmsg is a non-null return of CMSG_FIRSTHDR.
            (*cmsg).cmsg_level = level;
            (*cmsg).cmsg_type = typ;
            (*cmsg).cmsg_len = max_len.try_into().unwrap();

            // Safety (for new_unchecked): we check above the cmsg is not null.
            // Invariants: cmsg is alligned and dereferenceable because it comes
            // from CMSG_FIRSTHDR and it is initialized above. cmsg_len is
            // calculated to ensure the byte buffer starting at cmsg is within
            // the msg_control buffer which is valid for reads and writes (and
            // is initialized as a byte buffer so the remaining bytes after the
            // cmsghdr are initialized as bytes).
            Some(Self {
                cmsg: NonNull::new_unchecked(cmsg),
                _phantom: PhantomData,
            })
        }
    }

    fn shrink_data_len(&mut self, len: c_uint) {
        // Safety: CMSG_LEN is safe for any input.
        let cmsg_len = unsafe { CMSG_LEN(len) }.try_into().unwrap();
        // Safety: from the invariant self.cmsg is properly aligned, dereferenceable
        // and has been initalized. The only pointer to the memory pointed to
        // by self.cmsg is self.cmsg so the only way to read/write this memory
        // for the rest of this method is through cmsg.
        let mut cmsg = unsafe { self.cmsg.as_mut() };
        if cmsg_len < cmsg.cmsg_len {
            // Invariant: shrinking cmsg.cmsg_len maintains the invariant
            // that the bytes buffer pointed to by cmsg is valid for reads
            // and writes for cmsg.cmsg_len bytes.
            cmsg.cmsg_len = cmsg_len;
        }
    }

    fn data(&mut self) -> &mut [u8] {
        // Safety: from the invariant self.cmsg is properly aligned, dereferenceable
        // and has been initalized. self.cmsg is the only pointer to the cmsghdr
        // that can be accessed.
        let cmsg = unsafe { self.cmsg.as_ref() };
        // cmsg points to a properly initalized and aligned cmsghdr.
        let data: *mut u8 = unsafe { CMSG_DATA(cmsg) };

        let hdr_size = (data as usize) - (cmsg as *const cmsghdr as usize);
        let data_size = (cmsg).cmsg_len - hdr_size;

        assert!(data_size <= (isize::MAX as usize));
        // Safety: CMSG_DATA means that data points to the data portion of the
        // cmsg (and is non-null). data_size is calculated as the size of the
        // whole cmsg less the part that is before data (the cmsghdr and any
        // padding). The invariant on self.cmsg means that the whole of byte
        // buffer starting from data for data_size bytes is valid for reads
        // and writes (because the byte buffer from cmsg for cmsg.cmsg_len
        // bytes is) and is initalized. Finally, the lifetime bounds on this
        // method and the invariant on self.cmsg that there are no other
        // pointers to the memory pointed to by self.cmsg mean that it is not
        // possible to access the memory in the returned slice except through
        // that slice so long as that slice is live.
        unsafe { slice::from_raw_parts_mut(data, data_size) }
    }
}

impl Fd {
    // Precondition: fd is the only retained copy of that RawFd.
    fn new(fd: RawFd) -> Self {
        // Invariant: the precondition means that this Fd can own the contained
        // RawFd because there are no other copies of that RawFd.
        Self { fd: Some(fd) }
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        if let Some(fd) = self.fd {
            // Safety: the invariant on self.fd means the owner of the
            // contained RawFd is about to be dropped.
            unsafe { close(fd) };
        }
    }
}

impl IntoRawFd for Fd {
    fn into_raw_fd(mut self) -> RawFd {
        self.fd
            .take()
            .expect("Attempt to take the RawFd contained in an Fd a second time")
    }
}

#[derive(Debug)]
struct CMsgBufferTooSmallError {}

impl CMsgBufferTooSmallError {
    fn new() -> io::Error {
        io::Error::new(io::ErrorKind::Other, CMsgBufferTooSmallError {})
    }
}

impl fmt::Display for CMsgBufferTooSmallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "The control buffer passed to MsgHdr was too small for the \
                    number of file descriptors"
        )
    }
}

impl error::Error for CMsgBufferTooSmallError {}

/// Returns the size needed for a msghdr control buffer big
/// enough to hold `count` `RawFd`'s.
pub fn cmsg_buffer_fds_space(count: usize) -> usize {
    // Safety: CMSG_SPACE is safe
    unsafe { CMSG_SPACE((count * mem::size_of::<RawFd>()) as u32) as usize }
}

fn call_res<F, R>(mut f: F) -> Result<R, io::Error>
where
    F: FnMut() -> R,
    R: One + Neg<Output = R> + PartialEq,
{
    let res = f();
    if res == -R::one() {
        Err(io::Error::last_os_error())
    } else {
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{iter, os::unix::io::AsRawFd};

    #[test]
    fn recv_end_take_fds_twice_is_empty() {
        let mut control_buffer = [0u8; 1024];
        let bufs: [IoSlice; 0] = [];
        let fds = [1, 2, 3, 4];
        let mhdr = MsgHdr::from_io_slice(&bufs, &mut control_buffer)
            .encode_fds(fds.iter().map(|fd| *fd))
            .expect("Can't encode fds");

        let mut sut = MsgHdrRecvEnd {
            mhdr: mhdr.mhdr,
            bytes_recvieved: 0,
            fds_taken: false,
            _phantom: PhantomData,
        };
        // the encoded fds are fake so don't really drop them
        mem::forget(sut.take_fds());
        let taken = sut.take_fds();

        assert_eq!(taken.count(), 0);
    }

    #[test]
    fn start_send_encode_fds_with_small_buffer_is_error() {
        let mut control_buffer = vec![0u8; cmsg_buffer_fds_space(1)];
        let bufs: [IoSlice; 0] = [];
        let fds = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

        let sut = MsgHdr::from_io_slice(&bufs, &mut control_buffer);
        let result = sut.encode_fds(fds.iter().map(|fd| *fd));

        assert!(result.is_err());
    }

    #[test]
    fn recv_end_take_fds_handle_non_scm_rights() {
        let mut control_buffer = vec![0u8; cmsg_buffer_fds_space(4) + cmsg_buffer_cred_size()];
        let fds = [1, 2, 3, 4];
        let bufs: [IoSlice; 0] = [];
        let mhdr = MsgHdr::from_io_slice(&bufs, &mut control_buffer);
        unsafe {
            let mut cmsg = CMSG_FIRSTHDR(&mhdr.mhdr);
            assert_ne!(cmsg, ptr::null_mut());
            encode_fake_cred(cmsg);
            cmsg = CMSG_NXTHDR(&mhdr.mhdr, cmsg);
            assert_ne!(cmsg, ptr::null_mut());
            encode_fds(cmsg, &fds);
        }
        let mut count = 0;

        let mut sut = MsgHdrRecvEnd {
            mhdr: mhdr.mhdr,
            bytes_recvieved: 0,
            fds_taken: false,
            _phantom: PhantomData,
        };
        for fd in sut.take_fds() {
            count += 1;
            // the encoded fds are fake so don't drop them
            let _ = fd.into_raw_fd();
        }

        assert_eq!(count, fds.len());
    }

    #[test]
    fn send_ready_send_on_non_socket_is_error() {
        let mut control_buffer = [0u8; 0];
        let bytes = [1u8, 2, 3, 4, 5];
        let bufs = [IoSlice::new(&bytes)];
        let file = tempfile::tempfile().expect("Can't get temporary file.");

        let sut = MsgHdr::from_io_slice(&bufs, &mut control_buffer)
            .encode_fds(iter::empty())
            .expect("Can't encode fds");
        let result = sut.send(file.as_raw_fd());

        assert!(result.is_err());
    }

    #[test]
    fn recv_start_recv_on_non_socket_is_error() {
        let mut control_buffer = [0u8; 0];
        let mut bytes = [1u8, 2, 3, 4, 5];
        let mut bufs = [IoSliceMut::new(&mut bytes)];
        let file = tempfile::tempfile().expect("Can't get temporary file.");

        let sut = MsgHdr::from_io_slice_mut(&mut bufs, &mut control_buffer);
        let result = sut.recv(file.as_raw_fd());

        assert!(result.is_err());
    }

    unsafe fn encode_fds(cmsg: *mut cmsghdr, fds: &[RawFd]) {
        let data_size = fds.len() * mem::size_of::<RawFd>();
        (*cmsg).cmsg_len = CMSG_LEN((data_size) as u32) as usize;
        (*cmsg).cmsg_level = SOL_SOCKET;
        (*cmsg).cmsg_type = SCM_RIGHTS;

        ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, CMSG_DATA(cmsg), data_size);
    }

    fn cmsg_buffer_cred_size() -> usize {
        unsafe { CMSG_SPACE(mem::size_of::<libc::ucred>() as u32) as usize }
    }

    unsafe fn encode_fake_cred(cmsg: *mut cmsghdr) {
        let data_size = mem::size_of::<libc::ucred>();
        (*cmsg).cmsg_len = CMSG_LEN((data_size) as u32) as usize;
        (*cmsg).cmsg_level = SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_CREDENTIALS;

        let fake_cred = libc::ucred {
            pid: 5,
            uid: 2,
            gid: 2,
        };
        ptr::copy_nonoverlapping(
            &fake_cred as *const libc::ucred as *const u8,
            CMSG_DATA(cmsg),
            data_size,
        );
    }
}
