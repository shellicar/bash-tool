# The conformance suite's fixed world: one bash, one set of test files, one
# userland. Built from the bash source tree, which is the build context:
#
#   docker build --platform linux/amd64 -t bash-conformance \
#     -f tools/conformance.Dockerfile ~/repos/gnu/bash
#
# Why an image at all: bash's test files drive sed, grep, awk and od, and BSD
# and GNU disagree about several of them. Running the suite on the host tested
# both shells against whatever that host happened to have, which moves. Worse
# than a wrong answer, it produced false matches, where both shells failed the
# same way and the gate called it a pass.
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc make libc6-dev ca-certificates autoconf \
 && rm -rf /var/lib/apt/lists/*

COPY . /src
WORKDIR /src

# The tests drive the shell under test through THIS_SH, so bash is built here
# rather than copied in: a macOS binary is no use, and the version has to be
# the one whose tests these are.
# distclean first: the source tree is a working clone that has been configured
# and built on the host, and its Mach-O object files and host Makefile are no
# use here. Errors are not suppressed, because a build that fails quietly is
# how you end up measuring against a shell you did not build.
# The touch is in dependency order: COPY does not preserve enough of the
# original timestamps for make to believe the autotools output is current, so
# it tries to regenerate configure from configure.ac and needs autoconf.
RUN (make distclean || true) \
 && touch aclocal.m4 configure config.h.in \
 && ./configure --silent \
 && touch config.h.in \
 && make -s -j"$(nproc)"

# recho, zecho and printenv are bash's own test helpers, found through `.` on
# PATH. Without them fourteen files fail for a reason that is about neither
# shell.
RUN cc -o tests/recho support/recho.c \
 && cc -o tests/zecho support/zecho.c \
 && cc -o tests/printenv support/printenv.c

WORKDIR /src/tests
