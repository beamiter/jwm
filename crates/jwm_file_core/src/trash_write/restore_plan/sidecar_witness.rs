//! Private read-only witness prototype for preparing an explicit Restore Original.
use crate::{TRASH_INFO_LIMIT, copy::content_observation};
use rustix::{
    fd::{AsRawFd, OwnedFd},
    fs::{self, AtFlags, FileType, Mode, OFlags},
    process::geteuid,
};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read},
};

pub(super) struct SidecarWitness {
    name: OsString,
    pin: OwnedFd,
    stamp: SidecarStamp,
    bytes: Vec<u8>,
}

#[derive(Eq, PartialEq)]
struct SidecarStamp {
    dev: u64,
    ino: u64,
    version: [i128; 5],
}

impl SidecarStamp {
    fn validate(stat: &fs::Stat) -> io::Result<Self> {
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || stat.st_nlink != 1
            || stat.st_uid != geteuid().as_raw()
            || Mode::from_raw_mode(stat.st_mode).intersects(Mode::WGRP | Mode::WOTH)
            || !u64::try_from(stat.st_size).is_ok_and(|size| size <= TRASH_INFO_LIMIT as u64)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Trash metadata is not a bounded, owned, single-link regular file",
            ));
        }
        Ok(Self {
            dev: stat.st_dev,
            ino: stat.st_ino,
            version: content_observation(stat),
        })
    }
}

impl SidecarWitness {
    pub(super) fn capture(info_directory: &OwnedFd, key: &OsStr) -> io::Result<Self> {
        if !crate::mkdir::single_name(key) || key.len() > 255 - ".trashinfo".len() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut name = key.to_owned();
        name.push(".trashinfo");
        let pin = fs::openat(
            info_directory,
            &name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stamp = SidecarStamp::validate(&fs::fstat(&pin)?)?;
        let bytes = read_pinned(&pin)?;
        let witness = Self {
            name,
            pin,
            stamp,
            bytes,
        };
        witness.verify(info_directory)?;
        Ok(witness)
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn name(&self) -> &OsStr {
        &self.name
    }

    pub(super) fn verify(&self, info_directory: &OwnedFd) -> io::Result<()> {
        if SidecarStamp::validate(&fs::fstat(&self.pin)?)? != self.stamp
            || read_pinned(&self.pin)? != self.bytes
            || SidecarStamp::validate(&fs::fstat(&self.pin)?)? != self.stamp
            || SidecarStamp::validate(&fs::statat(
                info_directory,
                &self.name,
                AtFlags::SYMLINK_NOFOLLOW,
            )?)? != self.stamp
        {
            return Err(io::Error::other("the selected Trash metadata changed"));
        }
        Ok(())
    }
}

fn read_pinned(pin: &OwnedFd) -> io::Result<Vec<u8>> {
    let fd = fs::open(
        format!("/proc/self/fd/{}", pin.as_raw_fd()),
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut bytes = Vec::new();
    File::from(fd)
        .take(TRASH_INFO_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > TRASH_INFO_LIMIT {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}
