#!/bin/sh
# Builds a test image per OS and runs atome's test matrix in each. Runs from
# Linux (arm64 or x86-64), macOS on Apple silicon, or Windows — from Git Bash
# or WSL, or with run.ps1 from PowerShell.
#
#   sh docker/run.sh                     # everything this host can run
#   sh docker/run.sh debian alpine       # just these
#   make docker-test OS="debian fedora"
#
# OSes: debian ubuntu fedora alpine windows-cross android macos windows.
#
# The Linux images run anywhere. The two native entries run only where they
# can and are otherwise skipped, not failed:
#
#   macos    runs natively, on a Mac only — no container can be macOS
#   windows  a real Windows container, only when Docker is in Windows-container
#            mode on a Windows host; everywhere else Windows is covered by
#            windows-cross (a MinGW build tested under Wine)
#
# Inside the images, tests that need something a container lacks skip
# themselves the same way: the device tests skip where there is no sound card,
# and Audio Units are only built on Apple targets.
#
# The source is mounted read-only; cargo's registry and each image's target
# directory live in named volumes, so a second run rebuilds only what changed.

set -u

cd "$(dirname "$0")/.."

crate=atome
host="$(uname -s)"
all_oses="debian ubuntu fedora alpine windows-cross android macos windows"
oses="${*:-$all_oses}"

summary=""
failed=0

pass() { summary="$summary
  PASS  $1"; }
fail() { summary="$summary
  FAIL  $1"; failed=1; }
skip() { summary="$summary
  SKIP  $1"; }

docker_os="$(docker version --format '{{.Server.Os}}' 2>/dev/null)"

# The source directory as Docker wants it. Git Bash on Windows rewrites any
# argument that looks like a POSIX path — /src included — unless told not to,
# and gives /c/Users/… where Docker needs C:/Users/….
source_dir="$(pwd)"
case "$host" in
    MINGW* | MSYS* | CYGWIN*)
        export MSYS_NO_PATHCONV=1
        source_dir="$(pwd -W)"
        ;;
esac

# Linux containers run at the host's own architecture unless an image says
# otherwise; Wine and the NDK are x86-64 only, and are emulated on arm64.
case "$(uname -m)" in
    arm64 | aarch64) native=linux/arm64 ;;
    *) native=linux/amd64 ;;
esac

# Whether this host can run an x86-64 container at all. Docker Desktop on a Mac
# or on Windows can; Docker Engine on an arm64 Linux box only once QEMU is
# registered, which is one command — named in the skip below. Asked once.
amd64_checked=""
can_run_amd64() {
    if [ -z "$amd64_checked" ]; then
        if [ "$native" = linux/amd64 ] ||
            docker run --rm --platform linux/amd64 alpine:3.20 true >/dev/null 2>&1; then
            amd64_checked=yes
        else
            amd64_checked=no
        fi
    fi
    [ "$amd64_checked" = yes ]
}

in_docker() {
    os="$1" dockerfile="$2" platform="$3" profile="$4"
    shift 4

    if [ "$docker_os" != linux ]; then
        if [ -z "$docker_os" ]; then
            fail "$os (Docker is not running)"
        else
            skip "$os (Docker is in $docker_os-container mode; Linux images need Linux-container mode)"
        fi
        return
    fi

    if [ "$platform" = linux/amd64 ] && ! can_run_amd64; then
        skip "$os (x86-64 only, and this $native host cannot emulate it — enable with: docker run --privileged --rm tonistiigi/binfmt --install amd64)"
        return
    fi

    image="$crate-test-$os"
    echo ""
    echo "######## $os: building $image"

    if ! docker build --platform "$platform" -t "$image" -f "docker/$dockerfile" "$@" docker; then
        fail "$os (image would not build)"
        return
    fi

    echo "######## $os: testing"
    if docker run --rm --platform "$platform" \
        -v "$source_dir:/src:ro" \
        -v "$crate-cargo-registry:/root/.cargo/registry" \
        -v "$crate-target-$os:/target" \
        "$image" sh /src/docker/test.sh "$profile"; then
        pass "$os"
    else
        fail "$os"
    fi
}

for os in $oses; do
    case "$os" in
        debian) in_docker debian linux-debian.Dockerfile "$native" linux --build-arg BASE=debian:bookworm ;;
        ubuntu) in_docker ubuntu linux-debian.Dockerfile "$native" linux --build-arg BASE=ubuntu:24.04 ;;
        fedora) in_docker fedora linux-fedora.Dockerfile "$native" linux ;;
        alpine) in_docker alpine linux-alpine.Dockerfile "$native" linux ;;
        windows-cross) in_docker windows-cross windows-cross.Dockerfile linux/amd64 windows ;;
        android) in_docker android android.Dockerfile linux/amd64 android ;;

        macos)
            if [ "$host" != Darwin ]; then
                skip "macos (native only — runs on a Mac; no container can be macOS)"
                continue
            fi
            echo ""
            echo "######## macos: testing natively"
            if sh docker/test.sh macos; then pass macos; else fail macos; fi
            ;;

        windows)
            if [ "$docker_os" != windows ]; then
                skip "windows (native only — needs Docker in Windows-container mode on a Windows host; windows-cross covers it here)"
                continue
            fi
            echo ""
            echo "######## windows: building $crate-test-windows"
            if docker build -t "$crate-test-windows" -f docker/windows.Dockerfile docker &&
                docker run --rm -v "$source_dir:C:\\src:ro" "$crate-test-windows"; then
                pass windows
            else
                fail windows
            fi
            ;;

        *)
            fail "$os (unknown — $all_oses)"
            ;;
    esac
done

echo ""
echo "$crate across operating systems, from $host:$summary"

exit "$failed"
