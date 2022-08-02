#!/bin/bash
set -o errexit

# Prerequisite: ../yew_frontend/pkg is built. Do this by "build-plain.sh" in yew_frontend.

export VIMBAC_LIBDIR="/opt/vimba/Vimba_6_0/VimbaC/DynamicLib/x86_64bit"
export PKG_CONFIG_PATH=/opt/libvpx/libvpx-1.8.0/lib/pkgconfig

PKG_CONFIG_PATH=/opt/libvpx/libvpx-1.8.0/lib/pkgconfig \
RUSTFLAGS="$RUSTFLAGS -C target-cpu=sandybridge -C codegen-units=1 -C link-args=-Wl,-rpath,/opt/vimba/Vimba_6_0/VimbaC/DynamicLib/x86_64bit" \
NUM_JOBS=2 \
cargo build --features "strand-cam/imtrack-absdiff strand-cam/bundle_files strand-cam/posix_sched_fifo backtrace" --release