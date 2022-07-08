#!/bin/sh
# cp ../alpine-make-rootfs/example-20220708.tar.gz ./ && sudo rm -fr ./alpine-rootfs && mkdir ./alpine-rootfs && pushd ./alpine-rootfs/ && tar zxvf ../example-20220708.tar.gz ./ && popd
# export RUST_LOG=info
# export RUST_BACKTRACE=full
./target/debug/remora --exe /bin/ash --rootfs ./alpine-rootfs