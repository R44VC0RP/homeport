//! Linux (ext4, btrfs, xfs, zfs). **Stub**: every filesystem function returns
//! [`unsupported`]. Process plumbing (`Sys`) is implemented.
//!
//! Contract for the implementation (see the porting brief, §1–§9). Shared
//! code already handles, for every platform:
//! - item ids: `IdScheme::Assigned` makes the index assign ids and keep
//!   `FileKey → id` in its `ids` table, including the continuity rule (a new
//!   key at a name whose previous key vanished in the same rescan keeps the
//!   id) and `replace_content` re-pointing the key;
//! - id → path: `Volume::path_of` returns `Ok(None)` here, so the index walks
//!   its own rows (`parent_id`, raw name) up to the share root and verifies
//!   the key of what it finds;
//! - display names, aliases for case/normalization collisions and escapes for
//!   non-UTF-8 names (`names.rs`); `RawName::from_display` NFC-normalizes new
//!   names (`NamePolicy::Posix`);
//! - startup: the index's indexed directories are passed to
//!   `Watcher::watch_dir` after `Watcher::start`.
//!
//! Crates: `rustix` (statx, getdents64 `RawDir`, openat2, renameat_with,
//! linkat, inotify, xattrs, FICLONE). Add it under
//! `[target.'cfg(target_os = "linux")'.dependencies]` in `homeportd/Cargo.toml`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use homeport_proto::{Kind, Os};
use tokio::sync::mpsc;

use super::{
    DirOps, Durability, Entry, ErrKind, FileKey, FileOps, FsCaps, FsOps, IdScheme, NamePolicy, RawName, ServiceOps,
    SetMeta, SysOps, ThumbnailerOps, TrashOps, VolumeOps, WatchBatch, WatcherOps, unix, unsupported,
};

/// Path operations.
pub struct Fs;

impl FsOps for Fs {
    const OS: Os = Os::Linux;
    const OS_NAME: &'static str = "Linux";
    const ID_SCHEME: IdScheme = IdScheme::Assigned;
    const NAME_POLICY: NamePolicy = NamePolicy::Posix;

    /// `statx(AT_FDCWD, path, AT_SYMLINK_NOFOLLOW | AT_STATX_DONT_SYNC,
    /// TYPE|MODE|INO|SIZE|MTIME|CTIME|BTIME|MNT_ID)`. Key: `vol` = `st_dev`,
    /// `id` = inode, `gen` = btime ns (0 if absent). `flags`: HIDDEN for
    /// dot-names is optional (Finder hides them anyway); READ_ONLY never.
    fn lstat(_path: &Path) -> io::Result<Entry> {
        Err(unsupported("lstat"))
    }

    /// `openat2(share_root_fd, rel, O_RDONLY|O_NOFOLLOW|O_CLOEXEC,
    /// RESOLVE_BENEATH|RESOLVE_NO_MAGICLINKS)` where possible, else `open`
    /// with `O_NOFOLLOW` (ELOOP → `ErrKind::Symlink`).
    fn open_read(_path: &Path) -> io::Result<File> {
        Err(unsupported("open_read"))
    }

    /// `user.com.apple.metadata:_kMDItemUserTags` (written verbatim by
    /// `apply_meta`); fall back to `user.xdg.tags` converted to a tags plist,
    /// else `None`.
    fn tags(_path: &Path) -> Option<Vec<u8>> {
        None
    }

    /// errno mapping is shared with macOS.
    fn classify(e: &io::Error) -> ErrKind {
        unix::classify(e)
    }

    /// Probe like Mutagen: create two names in a temp dir under the root
    /// (`a`/`A`, NFC/NFD `é`) and see whether they collide; ext4 casefold
    /// dirs (`chattr +F`) are case-insensitive. `symlinks`, `mode`: true;
    /// `xattrs`/`tags`: probe `user.*` (ENOTSUP on NFS/FUSE); `set_crtime`:
    /// false (Linux cannot set btime; keep a client-set value in the DB later);
    /// `hidden`: false.
    fn caps(_root: &Path) -> FsCaps {
        FsCaps { case_sensitive: true, normalization_sensitive: true, symlinks: true, mode: true, ..FsCaps::default() }
    }

    /// `.Trash-<uid>` / `.Trash` at filesystem roots, `lost+found`.
    fn skip(name: &OsStr) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let b = name.as_bytes();
        b == b"lost+found" || b == b".Trash" || b.starts_with(b".Trash-")
    }

    /// Temp file in the same directory (`O_TMPFILE` + `linkat` via
    /// `/proc/self/fd/N`, or a named `.homeport-*` temp), copy mode and
    /// `user.*` xattrs from the target, copy content, `apply_meta`, `fsync`,
    /// `renameat` over the target, `fsync(dirfd)`. Return the new entry: the
    /// index keeps the item id and re-points the key.
    fn replace_content(
        _dir: &Dir,
        _name: &OsStr,
        _upload: &File,
        _staging: &Dir,
        _meta: &SetMeta<'_>,
    ) -> io::Result<Entry> {
        Err(unsupported("replace_content"))
    }
}

/// An open directory: an `O_PATH|O_DIRECTORY` or `O_RDONLY|O_DIRECTORY` fd.
pub struct Dir;

impl DirOps for Dir {
    /// `open(path, O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)`; with
    /// `expect`, `statx` the fd and compare the key (`same_file`).
    fn open(_path: &Path, _expect: Option<&FileKey>) -> io::Result<Self> {
        Err(unsupported("Dir::open"))
    }

    /// `getdents64` (rustix `RawDir`, reuse a 64–256 KiB buffer), then
    /// `statx(dirfd, name, AT_SYMLINK_NOFOLLOW|AT_STATX_DONT_SYNC, ...)` per
    /// entry (thread pool of 4–8 for >1k entries; sort by `d_ino`). `d_type`
    /// may be `DT_UNKNOWN`, so always statx. Entries on another mount
    /// (`stx_mnt_id` differs) are reported as `Kind::Other`. Not io_uring.
    fn scan(&self) -> io::Result<Vec<Entry>> {
        Err(unsupported("scan"))
    }

    /// `statx(dirfd, name, AT_SYMLINK_NOFOLLOW, ...)`.
    fn stat(&self, _name: &OsStr) -> io::Result<Entry> {
        Err(unsupported("stat"))
    }

    /// Case-sensitive filesystems store names as given: return `name`. On a
    /// casefold directory, find the entry with the same inode in `scan`.
    fn stored_name(&self, _name: &OsStr) -> io::Result<RawName> {
        Err(unsupported("stored_name"))
    }

    /// `openat(dirfd, name, O_RDONLY|O_RDWR, O_NOFOLLOW|O_CLOEXEC)`.
    fn open_file(&self, _name: &OsStr, _write: bool) -> io::Result<File> {
        Err(unsupported("open_file"))
    }

    /// Dirs: `O_RDONLY|O_DIRECTORY`; symlinks: `O_PATH|O_NOFOLLOW` (metadata
    /// via `/proc/self/fd` or `*at` calls with `AT_SYMLINK_NOFOLLOW`; Linux
    /// cannot chmod a symlink, skip mode there).
    fn open_meta(&self, _name: &OsStr, _kind: Kind) -> io::Result<File> {
        Err(unsupported("open_meta"))
    }

    /// `mkdirat(dirfd, name, 0o755)`.
    fn mkdir(&self, _name: &OsStr) -> io::Result<()> {
        Err(unsupported("mkdir"))
    }

    /// `symlinkat(target, dirfd, name)`.
    fn symlink(&self, _target: &str, _name: &OsStr) -> io::Result<()> {
        Err(unsupported("symlink"))
    }

    /// `unlinkat(dirfd, name, 0)`.
    fn unlink(&self, _name: &OsStr) -> io::Result<()> {
        Err(unsupported("unlink"))
    }

    /// `renameat2(.., RENAME_NOREPLACE)`; EXDEV across btrfs subvolumes
    /// surfaces as `ErrKind::CrossDevice`.
    fn rename_noreplace(&self, _from: &OsStr, _to_dir: &Dir, _to: &OsStr) -> io::Result<()> {
        Err(unsupported("rename"))
    }

    /// `openat(O_CREAT|O_EXCL)` + `ioctl(FICLONE)` (btrfs, xfs reflink,
    /// OpenZFS ≥ 2.2 with block cloning); else `Unsupported`.
    fn clone_file(&self, _src: &File, _name: &OsStr) -> io::Result<()> {
        Err(unsupported("clone"))
    }
}

/// An open file descriptor.
pub struct File;

impl FileOps for File {
    /// `statx(fd, "", AT_EMPTY_PATH, ...)`.
    fn stat(&self) -> io::Result<Entry> {
        Err(unsupported("File::stat"))
    }

    /// `pread`.
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> io::Result<usize> {
        Err(unsupported("read"))
    }

    /// `pwrite`.
    fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<usize> {
        Err(unsupported("write"))
    }

    /// `ftruncate`.
    fn set_len(&self, _len: u64) -> io::Result<()> {
        Err(unsupported("truncate"))
    }

    /// `Full`: `fsync`; `Barrier`: `fdatasync`.
    fn sync(&self, _how: Durability) -> io::Result<()> {
        Err(unsupported("sync"))
    }

    /// mode: `fchmod`; times: `futimens` (mtime; crtime cannot be set:
    /// ignore, caps say so); hidden: ignore; xattrs: `fsetxattr("user." +
    /// name)` (skip on ENOTSUP/E2BIG, ext4 has one 4 KiB block); tags: write
    /// `user.com.apple.metadata:_kMDItemUserTags` and `user.xdg.tags`.
    fn apply_meta(&self, _m: &SetMeta<'_>) -> io::Result<()> {
        Err(unsupported("apply_meta"))
    }
}

/// A mounted filesystem.
pub struct Volume;

impl VolumeOps for Volume {
    /// `statx(path, STATX_MNT_ID)` + `st_dev`.
    fn of(_path: &Path) -> io::Result<Self> {
        Err(unsupported("serving files"))
    }

    /// `st_dev` of the share root (btrfs subvolumes and zfs datasets inside a
    /// share have other devs; their keys still work, ids are index-assigned).
    fn key(&self) -> u64 {
        0
    }

    /// The filesystem UUID (`/dev/disk/by-uuid`, or `statfs.f_fsid` for btrfs/zfs).
    fn stable_id(&self) -> String {
        String::new()
    }

    /// No unprivileged id → path call (`open_by_handle_at` needs
    /// `CAP_DAC_READ_SEARCH`): always `Ok(None)`, the index walks its rows.
    fn path_of(&self, _key: &FileKey) -> io::Result<Option<PathBuf>> {
        Ok(None)
    }
}

/// inotify on indexed directories only.
pub struct Watcher;

impl WatcherOps for Watcher {
    /// One inotify fd and a reader thread. Ignore `cursor` (there is none):
    /// send `Change::All` once so every indexed directory is rescanned and
    /// diffed against the index. Check `/proc/sys/fs/inotify/max_user_watches`
    /// and log headroom.
    fn start(_roots: &[PathBuf], _cursor: Option<&str>, _tx: mpsc::UnboundedSender<WatchBatch>) -> io::Result<Self> {
        Err(unsupported("watching"))
    }

    /// `inotify_add_watch(path, IN_CREATE|IN_DELETE|IN_MOVED_FROM|IN_MOVED_TO|
    /// IN_CLOSE_WRITE|IN_ATTRIB|IN_MODIFY|IN_DELETE_SELF|IN_MOVE_SELF|
    /// IN_ONLYDIR|IN_EXCL_UNLINK)`, then send `Change::Path(path)` (the
    /// create-then-watch race). Events map to `Change::Path(dir/name)`;
    /// coalesce IN_MODIFY per ~100 ms; `IN_Q_OVERFLOW` → `Change::All`;
    /// `IN_IGNORED`/`IN_DELETE_SELF`/`IN_UNMOUNT` → `Change::Path(dir)`.
    fn watch_dir(&self, _path: &Path) {}

    /// `inotify_rm_watch`.
    fn unwatch_dir(&self, _path: &Path) {}
}

/// freedesktop.org Trash.
pub struct Trash;

impl TrashOps for Trash {
    /// Home trash `$XDG_DATA_HOME/Trash` (`~/.local/share/Trash`) with
    /// `files/` and `info/`.
    fn open(_home: &Path, _data_dir: &Path) -> io::Result<Self> {
        Err(unsupported("trash"))
    }

    /// Same filesystem as the home trash → home trash; else
    /// `$topdir/.Trash/$uid` (sticky, not a symlink) or `$topdir/.Trash-$uid`.
    /// Write `info/<name>.trashinfo` with `O_EXCL` (Path=percent-encoded
    /// absolute path, DeletionDate) first, then `renameat2(NOREPLACE)` into
    /// `files/<name>`; on collision add ` 2`, ` 3`... Return the new path.
    fn trash(&self, _dir: &Dir, _name: &OsStr) -> io::Result<PathBuf> {
        Err(unsupported("trash"))
    }

    /// Under any trash `files/` dir this instance uses.
    fn contains(&self, _path: &Path) -> bool {
        false
    }

    /// `renameat2(NOREPLACE)` back, then remove the `.trashinfo`.
    fn put_back(&self, _from: &Path, _to_dir: &Dir, _to: &OsStr) -> io::Result<()> {
        Err(unsupported("put back"))
    }
}

/// Thumbnails.
pub struct Thumbnailer;

impl ThumbnailerOps for Thumbnailer {
    /// Set to true once `generate` works.
    const AVAILABLE: bool = false;

    fn wants(_name: &str) -> bool {
        false
    }

    /// First the freedesktop cache (`~/.cache/thumbnails/{large,x-large}/
    /// md5(file URI).png`, valid when `Thumb::MTime` matches). Then: `image` +
    /// `fast_image_resize` for raster formats, `ffmpeg -ss 1 -frames:v 1` for
    /// video, `pdftoppm` for PDF, or the system thumbnailers in
    /// `/usr/share/thumbnailers/*.thumbnailer` (`Exec=` with `%i %o %s`).
    async fn generate(_path: &Path, _px: u32, _scratch: &Path) -> Option<Vec<u8>> {
        None
    }
}

/// Process plumbing (implemented).
pub struct Sys;

fn xdg(var: &str, home: &Path, default: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(default))
}

impl SysOps for Sys {
    fn home() -> Option<PathBuf> {
        unix::home()
    }

    /// `$XDG_STATE_HOME/homeport/server` (`~/.local/state/homeport/server`).
    fn data_dir(home: &Path) -> PathBuf {
        xdg("XDG_STATE_HOME", home, ".local/state").join("homeport/server")
    }

    /// `$XDG_CONFIG_HOME/homeport` (`~/.config/homeport`).
    fn config_dir(home: &Path) -> PathBuf {
        xdg("XDG_CONFIG_HOME", home, ".config").join("homeport")
    }

    /// `$XDG_CACHE_HOME/homeport` (`~/.cache/homeport`).
    fn cache_dir(home: &Path) -> PathBuf {
        xdg("XDG_CACHE_HOME", home, ".cache").join("homeport")
    }

    /// None: shares come from `homeportd share add`.
    fn default_shares(_home: &Path) -> Vec<(String, PathBuf)> {
        Vec::new()
    }

    fn host_name() -> String {
        unix::host_name()
    }

    fn tailscale_candidates() -> Vec<PathBuf> {
        vec![PathBuf::from("/usr/bin/tailscale"), PathBuf::from("/usr/local/bin/tailscale")]
    }

    fn tailscale_command(bin: &Path) -> tokio::process::Command {
        tokio::process::Command::new(bin)
    }

    async fn shutdown_signal() -> io::Result<()> {
        unix::shutdown_signal().await
    }

    fn local_stamp() -> String {
        unix::local_stamp()
    }

    fn create_private_dir(path: &Path) -> io::Result<()> {
        unix::create_private_dir(path)
    }

    fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
        unix::write_private(path, bytes)
    }
}

/// systemd user unit.
pub struct Service;

impl ServiceOps for Service {
    /// Write `~/.config/systemd/user/homeportd.service` (`ExecStart=exe args`,
    /// `Restart=always`, `RestartSec=5`, `Type=simple` or `notify` with
    /// `sd-notify`), `systemctl --user daemon-reload`, `enable --now`, and
    /// tell the user to run `loginctl enable-linger $USER` so it runs without
    /// a login. With `print`, return the unit text only. Also suggest the
    /// `net.core.rmem_max/wmem_max=7500000` sysctl for QUIC.
    fn install(_exe: &Path, _args: &[OsString], _print: bool) -> io::Result<String> {
        Err(unsupported("service install"))
    }

    /// `systemctl --user disable --now homeportd`, remove the unit, daemon-reload.
    fn uninstall() -> io::Result<String> {
        Err(unsupported("service uninstall"))
    }

    /// `systemctl --user show homeportd -p ActiveState,SubState,MainPID`.
    fn status() -> io::Result<String> {
        Err(unsupported("service status"))
    }
}
