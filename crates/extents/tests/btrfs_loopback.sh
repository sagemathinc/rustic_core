#!/usr/bin/env bash
# Real-btrfs check of rustic_extents (needs sudo, mkfs.btrfs and chattr).
#   btrfs_loopback.sh EXTENT_SIG_BINARY [MOUNT_OPTIONS]
set -euo pipefail

bin=$(realpath "$1")
opts=${2:-}
work=$(mktemp -d)
mnt=$work/mnt
cleanup() {
  sudo umount "$mnt" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

truncate -s 1G "$work/img"
mkfs.btrfs -q "$work/img"
mkdir "$mnt"
sudo mount -o "loop${opts:+,$opts}" "$work/img" "$mnt"
sudo btrfs -q subvolume create "$mnt/src"
sudo chown "$(id -u):$(id -g)" "$mnt/src"
s=$mnt/src

head -c 3M /dev/urandom >"$s/a"
cp --reflink=always "$s/a" "$s/clone"
cp --reflink=always "$s/a" "$s/edited"
dd if=/dev/urandom of="$s/edited" bs=4K seek=100 count=1 conv=notrunc status=none
head -c 3M /dev/urandom >"$s/other"
echo tiny >"$s/inline"
truncate -s 1M "$s/holes"
dd if=/dev/urandom of="$s/holes" bs=4K seek=200 count=1 conv=notrunc status=none
fallocate -l 1M "$s/prealloc"
touch "$s/nocow"
chattr +C "$s/nocow"
head -c 1M /dev/urandom >"$s/nocow"
: >"$s/empty"
# many extent items, to page through TREE_SEARCH results (about 770 per page)
for i in $(seq 0 1999); do
  dd if=/dev/urandom of="$s/many" bs=4K seek="$i" count=1 conv=notrunc,fsync status=none
  dd if=/dev/urandom of="$s/spacer" bs=4K seek="$i" count=1 conv=notrunc,fsync status=none
done
cp --reflink=always "$s/many" "$s/many-clone"
mkdir "$s/dir"
cp --reflink=always "$s/a" "$s/dir/a"
sudo btrfs -q subvolume create "$s/nested"
sudo cp --reflink=always "$s/a" "$s/nested/a"
sudo btrfs -q subvolume snapshot -r "$s" "$mnt/ro"

files="a clone edited other inline holes prealloc nocow empty many many-clone dir/a nested/a"
# shellcheck disable=SC2086
sudo "$bin" "$mnt/ro" $files >"$work/out"
cut -c1-100 "$work/out"

sig() { awk -v f="$1" '$1 == f { print $2 }' "$work/out"; }
fail() {
  echo "FAIL: $*"
  exit 1
}
# extents in a signature: (bytes - 20 tag - 16 fsid - 8 size - 4 count) / 61
extents() { echo $(((${#1} / 2 - 48) / 61)); }

[ "$(sig a)" != - ] || fail "a has no signature"
[ "$(sig a)" = "$(sig clone)" ] || fail "a reflink clone has a different signature"
[ "$(sig a)" = "$(sig dir/a)" ] || fail "a clone in a subdirectory differs"
[ "$(sig a)" != "$(sig edited)" ] || fail "a partially rewritten clone kept the signature"
[ "$(sig edited)" != - ] || fail "edited has no signature"
[ "$(sig a)" != "$(sig other)" ] || fail "different data, same signature"
for f in inline holes prealloc nocow empty nested/a; do
  [ "$(sig "$f")" = - ] || fail "$f must have no signature"
done
[ "$(sig many)" = "$(sig many-clone)" ] || fail "many-extent clone differs"
n=$(extents "$(sig many)")
[ "$n" -ge 1000 ] || fail "expected >= 1000 extents in many, got $n"
grep -qx 'end: Ok(())' "$work/out" || fail "end check"

"$bin" "$mnt/ro" a | grep -qx 'no-op: unprivileged' || fail "unprivileged run"
sudo "$bin" "$s" a | grep -qx 'no-op: source not read-only' || fail "writable source"

echo "OK: extent signatures on btrfs${opts:+ ($opts)}, many=$n extents"
