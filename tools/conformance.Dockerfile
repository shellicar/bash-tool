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
 && apt-get install -y --no-install-recommends gcc make libc6-dev ca-certificates autoconf locales \
 && rm -rf /var/lib/apt/lists/*

# Every locale bash's own tests ask for, found by sweeping tests/ for locale
# names. Without them the locale-sensitive files warn and skip on both shells
# at once, which is a false match rather than a false failure: the gate calls
# it a pass and nothing was compared.
RUN printf '%s\n' \
      'en_US.UTF-8 UTF-8' \
      'de_DE.UTF-8 UTF-8' \
      'fr_FR.ISO-8859-1 ISO-8859-1' \
      'ja_JP.SJIS SHIFT_JIS' \
      'ru_RU.CP1251 CP1251' \
      'zh_TW.BIG5 BIG5' \
      'zh_HK.BIG5-HKSCS BIG5-HKSCS' \
      > /etc/locale.gen \
 && locale-gen

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

# Bash's own tests refuse to run as root: several call the suite off with "the
# test suite should not be run as root". That hits both shells equally so the
# comparison stays fair, but those files then measure almost nothing. The tests
# write into their own directory as well as TMPDIR, so the tree is handed over
# with them.
RUN useradd --create-home --uid 1000 tester && chown -R tester /src
USER tester

WORKDIR /src/tests
