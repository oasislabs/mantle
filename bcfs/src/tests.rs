#![cfg(test)]

use std::{
    borrow::Cow,
    collections::HashMap,
    io::{IoSlice, IoSliceMut},
    path::{Path, PathBuf},
};

use blockchain_traits::{Blockchain, TransactionOutcome};
use memchain::{Account, Memchain};
use oasis_types::{Address, Event};
use wasi_types::{ErrNo, Fd, FdFlags, FileType, OpenFlags, Whence};

use crate::{
    file::{CHAIN_DIR_FILENO, HOME_DIR_FILENO},
    BCFS,
};

macro_rules! chain_name {
    () => {
        "testchain"
    };
    (opt) => {
        "/opt/testchain"
    };
}

const ADDR_1: Address = Address([1u8; 20]);
const ADDR_2: Address = Address([2u8; 20]);
const BASE_GAS: u64 = 2100;
const GAS_PRICE: u64 = 0;
const CHAIN_NAME: &str = chain_name!();

fn giga(val: u128) -> u128 {
    val * 1_000_000_000
}

fn create_memchain(mains: Vec<Option<memchain::AccountMain>>) -> Memchain<'static> {
    let genesis_state = mains
        .into_iter()
        .enumerate()
        .map(|(i, main)| {
            let i = i + 1;
            (
                Address([i as u8; 20]),
                Cow::Owned(Account {
                    balance: giga(i as u128),
                    code: format!("\0asm not wasm {}", i).into_bytes(),
                    storage: {
                        let mut storage = HashMap::new();
                        storage.insert(
                            "common_key".to_string().into_bytes(),
                            "common_value".to_string().into_bytes(),
                        );
                        storage.insert(
                            format!("key_{}", i).into_bytes(),
                            format!("value_{}", i).into_bytes(),
                        );
                        storage
                    },
                    expiry: None,
                    main,
                }),
            )
        })
        .collect();

    Memchain::new(CHAIN_NAME, genesis_state, BASE_GAS)
}

/// Returns a known-good home directory.
/// Path is relative to the FD with number CHAIN_DIR_FILENO.
fn good_home() -> PathBuf {
    PathBuf::from(hex::encode(&ADDR_2))
}

macro_rules! testcase {
    (fn $fn_name:ident ( $ptx:ident : &mut dyn PendingTransaction ) $body:block) => {
        #[test]
        fn $fn_name() {
            extern "C" fn test_main(ptxp: memchain::PtxPtr) -> u16 {
                let $ptx = unsafe { &mut **ptxp };
                $body
                0
            }
            let mut bc = create_memchain(vec![None, Some(test_main)]);
            let receipt = bc.last_block_mut().transact(
                ADDR_1, ADDR_2, ADDR_1, /* payer */
                42,     /* value */
                b"input", BASE_GAS, GAS_PRICE,
            );
            assert_eq!(receipt.outcome(), TransactionOutcome::Success);
        }
    };
}

testcase!(
    fn close_fd(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);
        for fd in 0u32..=3 {
            let fd = Fd::from(fd);
            assert!(bcfs.close(ptx, fd).is_ok());
            assert_eq!(bcfs.close(ptx, fd), Err(ErrNo::BadF)); // double close
        }
        for fd in (crate::file::HOME_DIR_FILENO + 1)..10 {
            assert_eq!(bcfs.close(ptx, Fd::from(fd)), Err(ErrNo::BadF));
        }
    }
);

testcase!(
    fn open_close(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);
        let mut abspath = good_home();
        abspath.push(".");
        abspath.push(".");
        abspath.push("somefile");
        abspath.push("..");
        abspath.push("somefile");
        let relpath = PathBuf::from("./././././somefile/../somefile/.");

        let abs_fd = bcfs
            .open(
                ptx,
                crate::file::CHAIN_DIR_FILENO.into(),
                &abspath,
                OpenFlags::CREATE,
                FdFlags::empty(),
            )
            .unwrap();

        // double create
        assert_eq!(
            bcfs.open(
                ptx,
                crate::file::CHAIN_DIR_FILENO.into(),
                &abspath,
                OpenFlags::EXCL,
                FdFlags::empty()
            ),
            Err(ErrNo::Exist)
        );

        let abs_fd2 = bcfs
            .open(
                ptx,
                crate::file::CHAIN_DIR_FILENO.into(),
                &abspath,
                OpenFlags::empty(),
                FdFlags::empty(),
            )
            .unwrap();
        let rel_fd = bcfs
            .open(
                ptx,
                crate::file::HOME_DIR_FILENO.into(),
                &relpath,
                OpenFlags::empty(),
                FdFlags::APPEND,
            )
            .unwrap();

        assert!(bcfs.close(ptx, abs_fd).is_ok());
        assert!(bcfs.close(ptx, abs_fd2).is_ok());
        assert!(bcfs.close(ptx, rel_fd).is_ok());
    }
);

testcase!(
    fn read_write_basic(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);

        let path = PathBuf::from("somefile");

        let fd = bcfs
            .open(
                ptx,
                crate::file::HOME_DIR_FILENO.into(),
                &path,
                OpenFlags::CREATE,
                FdFlags::empty(),
            )
            .unwrap();

        let write_bufs = ["hello", "world"];
        let mut read_bufs = write_bufs
            .iter()
            .map(|b| vec![0u8; b.len()])
            .collect::<Vec<_>>();
        let nbytes = write_bufs.iter().map(|b| b.len()).sum();

        macro_rules! assert_read {
            ($read_bufs:ident, $write_bufs:ident, $nbytes:expr) => {
                assert_eq!(
                    bcfs.read_vectored(
                        ptx,
                        fd,
                        &mut $read_bufs
                            .iter_mut()
                            .map(|b| IoSliceMut::new(b))
                            .collect::<Vec<_>>()
                    ),
                    Ok($nbytes)
                );
                assert!(
                    $nbytes == 0
                        || std::str::from_utf8(&$read_bufs[0]).unwrap() == $write_bufs[0]
                            && std::str::from_utf8(&$read_bufs[1]).unwrap() == $write_bufs[1]
                );
            };
        }
        assert_read!(read_bufs, write_bufs, 0);

        assert_eq!(
            bcfs.write_vectored(
                ptx,
                fd,
                &write_bufs
                    .iter()
                    .map(|b| IoSlice::new(b.as_bytes()))
                    .collect::<Vec<_>>()
            ),
            Ok(nbytes)
        );
        assert_read!(read_bufs, write_bufs, 0);

        assert_eq!(bcfs.seek(ptx, fd, 0, Whence::Start), Ok(0));
        assert_read!(read_bufs, write_bufs, nbytes);
        assert_read!(read_bufs, write_bufs, 0);

        assert_eq!(bcfs.seek(ptx, fd, -(nbytes as i64), Whence::End), Ok(0));
        assert_read!(read_bufs, write_bufs, nbytes);
        assert_read!(read_bufs, write_bufs, 0);

        assert_eq!(
            bcfs.seek(ptx, fd, -(nbytes as i64 - 2), Whence::Current),
            Ok(2)
        );
        assert_eq!(bcfs.seek(ptx, fd, -2, Whence::Current), Ok(0));

        assert_eq!(bcfs.seek(ptx, fd, 0, Whence::End), Ok(nbytes as u64));
        let write_bufs = ["hello", "blockchain"];
        let mut read_bufs = write_bufs
            .iter()
            .map(|b| vec![0u8; b.len()])
            .collect::<Vec<_>>();
        let new_nbytes = write_bufs.iter().map(|b| b.len()).sum();
        assert_eq!(
            bcfs.pwrite_vectored(
                ptx,
                fd,
                &write_bufs
                    .iter()
                    .map(|b| IoSlice::new(b.as_bytes()))
                    .collect::<Vec<_>>(),
                0
            ),
            Ok(new_nbytes)
        );
        assert_eq!(bcfs.tell(ptx, fd), Ok(nbytes as u64));
        assert_eq!(
            bcfs.pread_vectored(
                ptx,
                fd,
                &mut read_bufs
                    .iter_mut()
                    .map(|b| IoSliceMut::new(b))
                    .collect::<Vec<_>>(),
                0
            ),
            Ok(new_nbytes)
        );
        assert_eq!(std::str::from_utf8(&read_bufs[0]).unwrap(), write_bufs[0]);
        assert_eq!(std::str::from_utf8(&read_bufs[1]).unwrap(), write_bufs[1]);
    }
);

testcase!(
    fn write_consecutive(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);

        let path = PathBuf::from("somefile");

        let fd = bcfs
            .open(
                ptx,
                crate::file::HOME_DIR_FILENO.into(),
                &path,
                OpenFlags::CREATE,
                FdFlags::empty(),
            )
            .unwrap();

        let write_bufs: &[&[u8]] = &[b"hello", b" world"];

        let nbytes = write_bufs.iter().map(|buf| buf.len()).sum();

        let mut read_buf = vec![0u8; nbytes];

        for wb in write_bufs {
            assert_eq!(
                bcfs.write_vectored(ptx, fd, &[IoSlice::new(wb)]),
                Ok(wb.len())
            );
        }

        assert_eq!(
            bcfs.seek(ptx, fd, -(nbytes as i64) + 1, Whence::Current),
            Ok(1)
        );

        assert_eq!(
            bcfs.read_vectored(ptx, fd, &mut [IoSliceMut::new(&mut read_buf)]),
            Ok(nbytes - 1)
        );

        assert_eq!(
            read_buf[..nbytes - 1],
            write_bufs
                .iter()
                .flat_map(|buf| buf.iter().cloned())
                .collect::<Vec<u8>>()[1..]
        );
        assert_eq!(read_buf[read_buf.len() - 1], 0);
    }
);

testcase!(
    // 1. Open two fds pointing to a single file--one absolute, one relative
    // 2. Write "helloworld" into the file through the abs fd
    // 3. Write "!" into the file through the rel fd
    // 4. Flush the abs fd. The file now contains "helloworld".
    // 5. Read from the rel fd. It has an offset of 1 and will pull in "elloworld".
    // 6. Flush the rel fd. The file now (still) contains "helloworld".
    //    NB: This differs from POSIX which would maintain a separate write buffer
    //        In this context, it would incur undue overhead.
    // 7. Seek to beginning using abs fd and read file. Should be "!elloworld".
    fn read_write_aliased(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);

        let path = PathBuf::from("somefile");
        let abspath = good_home().join(&path);

        let rel_fd = bcfs
            .open(
                ptx,
                crate::file::HOME_DIR_FILENO.into(),
                &path,
                OpenFlags::CREATE,
                FdFlags::empty(),
            )
            .unwrap();
        let abs_fd = bcfs
            .open(
                ptx,
                crate::file::CHAIN_DIR_FILENO.into(),
                &abspath,
                OpenFlags::empty(),
                FdFlags::empty(),
            )
            .unwrap();

        let write_bufs = ["hello", "world"];
        let mut read_bufs = write_bufs
            .iter()
            .map(|b| vec![0u8; b.len()])
            .collect::<Vec<_>>();
        let nbytes = write_bufs.iter().map(|b| b.len()).sum();

        assert_eq!(
            bcfs.write_vectored(
                ptx,
                abs_fd, // NB: absolute path fd
                &write_bufs
                    .iter()
                    .map(|b| IoSlice::new(b.as_bytes()))
                    .collect::<Vec<_>>()
            ),
            Ok(nbytes)
        );
        let rel_seek = 1;
        assert_eq!(
            bcfs.write_vectored(
                ptx,
                rel_fd, // NB: relative path fd
                &[IoSlice::new(b"!")]
            ),
            Ok(rel_seek)
        );
        bcfs.flush(ptx, abs_fd).unwrap();
        assert_eq!(
            bcfs.read_vectored(
                ptx,
                rel_fd, // NB: relative path fd
                &mut read_bufs
                    .iter_mut()
                    .map(|b| IoSliceMut::new(b))
                    .collect::<Vec<_>>()
            ),
            Ok(nbytes - rel_seek)
        );
        assert_eq!(std::str::from_utf8(&read_bufs[0]).unwrap(), "ellow");
        assert_eq!(std::str::from_utf8(&read_bufs[1]).unwrap(), "orld\0");

        bcfs.flush(ptx, rel_fd).unwrap();

        bcfs.seek(ptx, abs_fd, 0, Whence::Start).unwrap();
        assert_eq!(
            bcfs.read_vectored(
                ptx,
                abs_fd, // NB: absolute path fd
                &mut read_bufs
                    .iter_mut()
                    .map(|b| IoSliceMut::new(b))
                    .collect::<Vec<_>>()
            ),
            Ok(nbytes)
        );
        assert_eq!(std::str::from_utf8(&read_bufs[0]).unwrap(), "hello");
        assert_eq!(std::str::from_utf8(&read_bufs[1]).unwrap(), "world");
    }
);

testcase!(
    fn badf(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);
        let badf = Fd::from(99u32);

        assert_eq!(
            bcfs.read_vectored(ptx, badf, &mut Vec::new()),
            Err(ErrNo::BadF)
        );

        assert_eq!(
            bcfs.write_vectored(ptx, badf, &Vec::new()),
            Err(ErrNo::BadF)
        );

        assert_eq!(
            bcfs.pread_vectored(ptx, badf, &mut Vec::new(), 0),
            Err(ErrNo::BadF)
        );

        assert_eq!(
            bcfs.pwrite_vectored(ptx, badf, &Vec::new(), 0),
            Err(ErrNo::BadF)
        );

        assert_eq!(bcfs.seek(ptx, badf, 0, Whence::Start), Err(ErrNo::BadF));

        assert_eq!(bcfs.fdstat(ptx, badf).unwrap_err(), ErrNo::BadF);
        assert_eq!(bcfs.filestat(ptx, badf).unwrap_err(), ErrNo::BadF);
        assert_eq!(bcfs.tell(ptx, badf).unwrap_err(), ErrNo::BadF);
        assert_eq!(bcfs.renumber(ptx, badf, badf).unwrap_err(), ErrNo::BadF);

        assert_eq!(bcfs.close(ptx, badf), Err(ErrNo::BadF));
    }
);

testcase!(
    fn renumber(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);
        let somefile = PathBuf::from("somefile");
        let anotherfile = PathBuf::from("anotherfile");

        let somefile_fd = bcfs
            .open(
                ptx,
                crate::file::HOME_DIR_FILENO.into(),
                &somefile,
                OpenFlags::CREATE,
                FdFlags::empty(),
            )
            .unwrap();
        let anotherfile_fd = bcfs
            .open(
                ptx,
                crate::file::HOME_DIR_FILENO.into(),
                &anotherfile,
                OpenFlags::CREATE,
                FdFlags::empty(),
            )
            .unwrap();

        let write_bufs = ["destination", "somefile"];
        bcfs.write_vectored(
            ptx,
            somefile_fd,
            &write_bufs
                .iter()
                .map(|b| IoSlice::new(b.as_bytes()))
                .collect::<Vec<_>>(),
        )
        .unwrap();

        bcfs.renumber(ptx, somefile_fd, anotherfile_fd).unwrap();

        assert_eq!(
            bcfs.read_vectored(ptx, somefile_fd, &mut Vec::new()),
            Err(ErrNo::BadF)
        );

        let mut read_buf = vec![0u8; 1];
        assert_eq!(
            bcfs.pread_vectored(
                ptx,
                anotherfile_fd,
                &mut [IoSliceMut::new(&mut read_buf)],
                0,
            ),
            Ok(read_buf.len())
        );
        assert_eq!(
            bcfs.tell(ptx, anotherfile_fd),
            Ok(write_bufs.iter().map(|b| b.len() as u64).sum())
        );
    }
);

testcase!(
    fn unlink(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);

        let path = PathBuf::from("somefile");
        let curdir = crate::file::HOME_DIR_FILENO.into();

        let fd = bcfs
            .open(ptx, curdir, &path, OpenFlags::CREATE, FdFlags::empty())
            .unwrap();

        let write_val = b"not empty";
        assert_eq!(
            bcfs.write_vectored(ptx, fd, &[std::io::IoSlice::new(write_val.as_ref())]),
            Ok(write_val.len())
        );
        assert!(bcfs.close(ptx, fd).is_ok());
        assert_eq!(bcfs.unlink(ptx, curdir, &path), Ok(write_val.len() as u64));
        assert_eq!(
            bcfs.open(ptx, curdir, &path, OpenFlags::empty(), FdFlags::empty()),
            Err(ErrNo::NoEnt)
        );
    }
);

macro_rules! write_twice {
    ($ptx:ident, $oflags:expr, $fdflags:expr, $expected:expr) => {{
        let mut bcfs = BCFS::new(*$ptx.address(), CHAIN_NAME);

        let path = PathBuf::from("somefile");
        let curdir = crate::file::HOME_DIR_FILENO.into();

        let first = "some initial, rather lengthy value";
        let second = "a second value";

        let mut do_write = |val: &str| {
            let fd = bcfs
                .open($ptx, curdir, &path, OpenFlags::CREATE | $oflags, $fdflags)
                .unwrap();
            bcfs.write_vectored($ptx, fd, &[std::io::IoSlice::new(val.as_bytes())])
                .unwrap();
            bcfs.close($ptx, fd).unwrap();
        };

        do_write(first);
        do_write(second);

        let fd = bcfs
            .open($ptx, curdir, &path, OpenFlags::empty(), FdFlags::empty())
            .unwrap();

        let mut read_buf = vec![' ' as u8; first.len() + second.len()];
        bcfs.read_vectored($ptx, fd, &mut [IoSliceMut::new(&mut read_buf)])
            .unwrap();

        assert_eq!(
            String::from_utf8(read_buf).unwrap().trim(),
            $expected(first, second)
        );
    }};
}

testcase!(
    fn write_trunc(ptx: &mut dyn PendingTransaction) {
        write_twice!(ptx, OpenFlags::TRUNC, FdFlags::empty(), |_first, second| {
            second
        });
    }
);

testcase!(
    fn write_append(ptx: &mut dyn PendingTransaction) {
        write_twice!(
            ptx,
            OpenFlags::empty(),
            FdFlags::APPEND,
            |first: &str, second| { first.to_string() + second }
        );
    }
);

testcase!(
    fn write_trunc_append(ptx: &mut dyn PendingTransaction) {
        write_twice!(ptx, OpenFlags::TRUNC, FdFlags::APPEND, |_first, second| {
            second
        });
    }
);

testcase!(
    fn prestat(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);

        // The following test is based on what the WASI libc does during initialization.
        // It starts after fd 3 (stdout) and queries each fd for its path (assuming it's
        // a directory) and stops after getting a badf.
        // Here, we expect two directories: the chain dir and the cwd.
        assert_eq!(
            bcfs.prestat(ptx, CHAIN_DIR_FILENO.into()).unwrap(),
            Path::new(chain_name!(opt))
        );
        assert_eq!(
            bcfs.prestat(ptx, HOME_DIR_FILENO.into()).unwrap(),
            Path::new(".")
        );

        assert_eq!(bcfs.prestat(ptx, Fd::from(5u32)), Err(ErrNo::BadF));
        assert_eq!(bcfs.prestat(ptx, Fd::from(6u32)), Err(ErrNo::BadF));
    }
);

testcase!(
    fn open_bad(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);

        // Opening from a non-directory root fd.
        assert_eq!(
            bcfs.open(
                ptx,
                Fd::from(0u32 /* stdin */),
                &Path::new("file.txt"),
                OpenFlags::empty(),
                FdFlags::empty()
            ),
            Err(ErrNo::BadF)
        );

        // Opening a file without a capability to its directory.
        assert_eq!(
            bcfs.open(
                ptx,
                HOME_DIR_FILENO.into(),
                &Path::new(chain_name!(opt)),
                OpenFlags::empty(),
                FdFlags::empty()
            ),
            Err(ErrNo::NoEnt)
        );

        // Opening a file above its directory.
        assert_eq!(
            bcfs.open(
                ptx,
                HOME_DIR_FILENO.into(),
                &Path::new("../../../asdf"),
                OpenFlags::empty(),
                FdFlags::empty()
            ),
            Err(ErrNo::NoEnt)
        );

        // Opening nothing.
        assert_eq!(
            bcfs.open(
                ptx,
                HOME_DIR_FILENO.into(),
                &Path::new(""),
                OpenFlags::empty(),
                FdFlags::empty()
            ),
            Err(ErrNo::Inval)
        );

        // Opening a non-existent file in a non-home directory that does exist.
        assert_eq!(
            bcfs.open(
                ptx,
                CHAIN_DIR_FILENO.into(),
                &Path::new("not-log"),
                OpenFlags::empty(),
                FdFlags::empty()
            ),
            Err(ErrNo::NoEnt)
        );

        // Blockchain special files vs invaid open flags.
        for (f, dir) in [
            ("balance", HOME_DIR_FILENO.into()), // 4 is home dir
            ("log", CHAIN_DIR_FILENO.into()),    // 3 is chain dir
            ("bytecode", HOME_DIR_FILENO.into()),
        ]
        .iter()
        {
            // Specials already exist and cannot be created.
            for oflags in [
                OpenFlags::CREATE,
                OpenFlags::EXCL,
                OpenFlags::CREATE | OpenFlags::EXCL,
            ]
            .iter()
            {
                assert_eq!(
                    bcfs.open(ptx, *dir, &Path::new(f), *oflags, FdFlags::empty()),
                    Err(ErrNo::Exist)
                );
            }

            // Special files cannot be truncated.
            assert_eq!(
                bcfs.open(ptx, *dir, &Path::new(f), OpenFlags::TRUNC, FdFlags::empty()),
                Err(ErrNo::Inval)
            );

            // They can't be unlinked either.
            assert_eq!(bcfs.unlink(ptx, *dir, &Path::new(f)), Err(ErrNo::Access));
        }

        // Log is append-only.
        assert_eq!(
            bcfs.open(
                ptx,
                CHAIN_DIR_FILENO.into(),
                &Path::new("log"),
                OpenFlags::empty(),
                FdFlags::empty()
            ),
            Err(ErrNo::Inval)
        );
    }
);

#[test]
fn tempfile() {
    extern "C" fn test_main(ptxp: memchain::PtxPtr) -> u16 {
        let ptx = unsafe { &mut **ptxp };

        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);

        let temp_fd = bcfs.tempfile(ptx).unwrap();

        let write_buf = b"ephemeral";
        let mut read_buf = vec![0u8; write_buf.len()];

        bcfs.write_vectored(ptx, temp_fd, &[IoSlice::new(write_buf)])
            .unwrap();

        bcfs.seek(ptx, temp_fd, 0, Whence::Start).unwrap();

        bcfs.read_vectored(ptx, temp_fd, &mut [IoSliceMut::new(&mut read_buf)])
            .unwrap();
        assert_eq!(&read_buf, write_buf);

        bcfs.flush(ptx, temp_fd).unwrap();

        0
    }

    let mut bc = create_memchain(vec![None, Some(test_main)]);

    let state_before = bc
        .blocks
        .last()
        .unwrap()
        .state
        .get(&ADDR_2)
        .unwrap()
        .storage
        .clone();

    bc.last_block_mut().transact(
        ADDR_1, ADDR_2, ADDR_1, /* payer */
        0,      /* value */
        b"input", BASE_GAS, GAS_PRICE,
    );

    assert_eq!(
        bc.blocks
            .last()
            .unwrap()
            .state
            .get(&ADDR_2)
            .unwrap()
            .storage
            .clone(),
        state_before
    );
}

testcase!(
    fn flush_preopens(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);
        for i in 0u32..=4 {
            assert!(bcfs.flush(ptx, Fd::from(i)).is_ok());
        }
    }
);

testcase!(
    fn flush_specials(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);
        for f in ["balance", "bytecode"].iter() {
            let fd = bcfs
                .open(
                    ptx,
                    HOME_DIR_FILENO.into(),
                    &Path::new(f),
                    OpenFlags::empty(),
                    FdFlags::empty(),
                )
                .unwrap();
            assert!(bcfs.flush(ptx, fd).is_ok());
        }
        let fd = bcfs
            .open(
                ptx,
                CHAIN_DIR_FILENO.into(),
                &Path::new("log"),
                OpenFlags::empty(),
                FdFlags::APPEND,
            )
            .unwrap();
        assert!(bcfs.flush(ptx, fd).is_ok());
    }
);

#[test]
fn flush_log_to_ptx() {
    extern "C" fn test_main(ptxp: memchain::PtxPtr) -> u16 {
        let ptx = unsafe { &mut **ptxp };
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);

        let log_fd = bcfs
            .open(
                ptx,
                CHAIN_DIR_FILENO.into(),
                &Path::new("log"),
                OpenFlags::empty(),
                FdFlags::APPEND,
            )
            .unwrap();

        let (_topics, _data, log) = create_log();
        bcfs.write_vectored(ptx, log_fd, &[IoSlice::new(&log)])
            .unwrap();

        bcfs.flush(ptx, log_fd).unwrap();

        0
    }

    let mut bc = create_memchain(vec![None, Some(test_main)]);

    let receipt = bc.last_block_mut().transact(
        ADDR_1, ADDR_2, ADDR_1, /* payer */
        0,      /* value */
        b"input", BASE_GAS, GAS_PRICE,
    );

    let events = receipt.events();
    assert_eq!(events.len(), 1);
    let (topics, data, _log) = create_log();
    assert_eq!(
        *events[0],
        Event {
            emitter: ADDR_2,
            topics: topics
                .iter()
                .map(|t| {
                    let mut t_bytes = [0u8; 32];
                    let t_len = std::cmp::min(t.len(), t_bytes.len());
                    t_bytes[..t_len].copy_from_slice(&t[..t_len]);
                    t_bytes
                })
                .collect(),
            data,
        }
    )
}

#[test]
fn flush_output_to_ptx() {
    const OUTPUT: &[u8] = b"output";

    extern "C" fn test_main(ptxp: memchain::PtxPtr) -> u16 {
        let ptx = unsafe { &mut **ptxp };
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);

        let stdout_fd = Fd::from(1u32);

        bcfs.write_vectored(ptx, stdout_fd, &[IoSlice::new(&OUTPUT)])
            .unwrap();

        bcfs.flush(ptx, stdout_fd).unwrap();

        0
    }

    let mut bc = create_memchain(vec![None, Some(test_main)]);

    let receipt = bc.last_block_mut().transact(
        ADDR_1, ADDR_2, ADDR_1, /* payer */
        0,      /* value */
        b"input", BASE_GAS, GAS_PRICE,
    );

    assert_eq!(receipt.output(), OUTPUT);
}

testcase!(
    fn read_specials(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);

        let mut read_special = |name, dir_fd| {
            let fd = bcfs
                .open(
                    ptx,
                    dir_fd,
                    &Path::new(name),
                    OpenFlags::empty(),
                    FdFlags::empty(),
                )
                .unwrap();

            let mut read_buf = vec![0u8; 20];
            let nbytes = bcfs
                .read_vectored(ptx, fd, &mut [IoSliceMut::new(&mut read_buf)])
                .unwrap();
            bcfs.close(ptx, fd).unwrap();
            (read_buf, nbytes)
        };

        // The following expected values are set up in `create_memchain`.
        let expected_balance = 2000000042;
        let expected_bytecode = format!("\0asm not wasm {}", 2).into_bytes();

        let (balance_bytes, nbytes) = read_special("balance", HOME_DIR_FILENO.into());
        let mut balance_arr = [0u8; 16];
        balance_arr.copy_from_slice(&balance_bytes[..nbytes]);
        assert_eq!(u128::from_le_bytes(balance_arr), expected_balance);

        let (bytecode, nbytes) = read_special("bytecode", HOME_DIR_FILENO.into());
        assert_eq!(&bytecode[..nbytes], expected_bytecode.as_slice());

        for i in 1u32..=2 {
            // stdout and stderr are unreadable.
            let mut read_buf = vec![0u8; 0];
            assert_eq!(
                bcfs.read_vectored(ptx, i.into(), &mut [IoSliceMut::new(&mut read_buf)]),
                Err(ErrNo::Inval)
            );
        }
    }
);

testcase!(
    fn seek_then_read(ptx: &mut dyn PendingTransaction) {
        let mut bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);

        let path = Path::new("hello");

        let fd = bcfs
            .open(
                ptx,
                crate::file::HOME_DIR_FILENO.into(),
                &path,
                OpenFlags::CREATE,
                FdFlags::empty(),
            )
            .unwrap();

        let write_buf = b"seeked";
        bcfs.write_vectored(ptx, fd, &[IoSlice::new(write_buf)])
            .unwrap();
        bcfs.close(ptx, fd).unwrap();

        let mut seek_then_read = |offset, whence| {
            let fd = bcfs
                .open(
                    ptx,
                    crate::file::HOME_DIR_FILENO.into(),
                    &path,
                    OpenFlags::empty(),
                    FdFlags::empty(),
                )
                .unwrap();

            bcfs.seek(ptx, fd, offset as i64, whence).unwrap();
            assert_eq!(bcfs.tell(ptx, fd).unwrap(), offset as u64);

            let mut read_buf = vec![0u8; write_buf.len() - offset];
            bcfs.read_vectored(ptx, fd, &mut [IoSliceMut::new(&mut read_buf)])
                .unwrap();
            bcfs.close(ptx, fd).unwrap();

            assert_eq!(read_buf.as_slice(), &write_buf[offset..]);
        };

        seek_then_read(3, Whence::Start);
        seek_then_read(0, Whence::Current);
    }
);

testcase!(
    fn fdstat(ptx: &mut dyn PendingTransaction) {
        let bcfs = BCFS::new(*ptx.address(), CHAIN_NAME);
        assert_eq!(
            bcfs.fdstat(ptx, Fd::from(0u32)).unwrap().file_type,
            FileType::RegularFile
        );
        assert_eq!(
            bcfs.fdstat(ptx, CHAIN_DIR_FILENO.into()).unwrap().file_type,
            FileType::Directory
        );
        assert_eq!(bcfs.fdstat(ptx, Fd::from(99u32)), Err(ErrNo::BadF));
    }
);

pub fn create_log() -> (Vec<Vec<u8>>, Vec<u8>, Vec<u8>) {
    let topics = vec![b"hello".to_vec(), b"world".to_vec()];
    let num_topics_bytes = (topics.len() as u32).to_le_bytes();
    let topic_lens: Vec<Vec<u8>> = topics
        .iter()
        .map(|t| (t.len() as u32).to_le_bytes().to_vec())
        .collect();
    let topics_bytes: Vec<u8> = num_topics_bytes
        .iter()
        .chain(
            topics
                .iter()
                .enumerate()
                .flat_map(|(i, t)| topic_lens[i].iter().chain(t.iter())),
        )
        .copied()
        .collect();

    let data = b"I bid thee hello!".to_vec();
    let data_len = (data.len() as u32).to_le_bytes();

    let log: Vec<u8> = std::iter::empty()
        .chain(topics_bytes.iter())
        .chain(data_len.iter())
        .chain(data.iter())
        .copied()
        .collect();

    (topics, data, log)
}
