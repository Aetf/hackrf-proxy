#!/bin/sh
# Run the containerised `hrf` against the real HackRF.
#
#   deploy/hrf-podman.sh info
#   deploy/hrf-podman.sh capture --seconds 5 --out flame_up.cs8
#
# Captures land in ./captures on the host. All arguments are passed through.
set -eu

image=${HRF_IMAGE:-hackrf-proxyd}
captures=${HRF_CAPTURES:-$(pwd)/captures}
mkdir -p "$captures"

tty=""
[ -t 0 ] && [ -t 1 ] && tty="-it"

# The whole USB directory rather than a single --device node: the HackRF changes
# its device number every time it re-enumerates, which happens on re-plug and
# after a device reset, and a stale --device path fails in a way that looks like
# missing hardware. A bind mount of the directory picks the new node up on its
# own.
# --group-add keep-groups: the udev rule grants access through the `wheel`
# group, but a rootless container only maps the user's uid and primary gid, so
# supplementary group membership is dropped and the node stays unreadable even
# though the same user can open it on the host. This asks crun to keep the
# host's supplementary groups for the permission check.
exec podman run --rm $tty \
	--group-add keep-groups \
	-v /dev/bus/usb:/dev/bus/usb \
	-v "$captures":/captures \
	"$image" "$@"
