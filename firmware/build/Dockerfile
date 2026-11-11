# Corvus firmware build environment, mirroring the RFOne firmware's upstream CI:
# ubuntu-24.04 x86_64 + gcc-multilib (the 2013 toolchain is 32-bit) + Arm GCC 4.7-2013q3.
# The build context is firmware/build/dl/, which build.sh fills with the toolchain tarball
# (md5-checked there and again here). Newer GCCs are not validated for this code.
FROM ubuntu:24.04
# If Ubuntu's own archive is unreachable (it was, with Launchpad, on 2026-11-11), retry on the kernel.org
# mirror of it: the same packages, still checked against Ubuntu's signing keys.
RUN (apt-get update -qq || { sed -i 's@http://\(archive\|security\)\.ubuntu\.com/ubuntu@http://mirrors.edge.kernel.org/ubuntu@g' /etc/apt/sources.list.d/ubuntu.sources && apt-get update -qq; }) \
 && apt-get install -y --no-install-recommends gcc-multilib make bzip2 python3 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY gcc-arm-none-eabi-4_7-2013q3-20130916-linux.tar.bz2 /tmp/
RUN echo "c35b662e371f369619cf202692a4d10b  /tmp/gcc-arm-none-eabi-4_7-2013q3-20130916-linux.tar.bz2" | md5sum -c - \
 && tar -xjf /tmp/gcc-arm-none-eabi-4_7-2013q3-20130916-linux.tar.bz2 -C /opt \
 && rm /tmp/gcc-arm-none-eabi-4_7-2013q3-20130916-linux.tar.bz2
ENV PATH=/opt/gcc-arm-none-eabi-4_7-2013q3/bin:$PATH
