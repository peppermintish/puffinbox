#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  printf 'Usage: %s /path/to/ffmpeg /path/to/ffprobe\n' "$0" >&2
  exit 2
fi

for tool in "$1" "$2"; do
  if [ ! -x "$tool" ]; then
    printf 'Not an executable file: %s\n' "$tool" >&2
    exit 1
  fi
  if ! readelf -h "$tool" >/dev/null 2>&1; then
    printf 'Not a supported Linux ELF executable: %s\n' "$tool" >&2
    exit 1
  fi
  if ! readelf -h "$tool" 2>/dev/null | grep -Eq 'Class:[[:space:]]+ELF64' || \
     ! readelf -h "$tool" 2>/dev/null | grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64'; then
    printf 'Expected a 64-bit x86_64 Linux executable: %s\n' "$tool" >&2
    exit 1
  fi
  if readelf -l "$tool" 2>/dev/null | grep -q 'INTERP'; then
    printf 'Dynamic program interpreter found in %s; the default scratch image needs static tools.\n' "$tool" >&2
    exit 1
  fi
  if readelf -d "$tool" 2>/dev/null | grep -q '(NEEDED)'; then
    printf 'Dynamic library dependency found in %s; the default scratch image needs static tools.\n' "$tool" >&2
    exit 1
  fi
done

printf 'Both tools are static ELF64 x86_64 files and pass the default image architecture/linkage checks. This check does not establish their CPU instruction requirements or feature support.\n'
