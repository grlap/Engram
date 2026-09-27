# Sourced by test-rust.sh: what the Rust gate takes from its host, kept apart
# so that scripts/test-launcher.test.mjs can run both functions against
# stubbed hosts.

# Eight threads measured fastest of 4, 8, 12 and 24 on a 24-core host; see
# docs/development.md. Use fewer where fewer processors are available. nproc
# honours CPU affinity, which getconf and sysctl do not; it also honours the
# OpenMP thread variables, which say nothing about this gate, so it runs
# without them.
default_test_threads() {
    processors=$( (unset OMP_NUM_THREADS OMP_THREAD_LIMIT; nproc) 2>/dev/null \
        || getconf _NPROCESSORS_ONLN 2>/dev/null \
        || sysctl -n hw.ncpu 2>/dev/null \
        || echo 4)
    case "$processors" in
        ''|*[!0-9]*|0) processors=4 ;;
    esac
    if [ "$processors" -gt 8 ]; then
        echo 8
    else
        echo "$processors"
    fi
}

# Reads current_soft_limit and desired_soft_limit. Best effort: it asks for
# the desired limit and, when the host refuses, for smaller ones, largest
# first, never for the same limit twice. A host can refuse the target although
# its hard limit reads "unlimited": macOS caps a process at
# kern.maxfilesperproc. It never asks for more than the desired limit. It
# succeeds as soon as the host accepts a request or the inherited limit
# already reaches the next candidate, and fails only when neither happened for
# any candidate; the inherited limit then stays in place and the caller warns.
raise_fd_soft_limit() {
    per_process_maximum=$(sysctl -n kern.maxfilesperproc 2>/dev/null || true)
    case "$per_process_maximum" in
        ''|*[!0-9]*|0)
            candidates="$desired_soft_limit 4096"
            ;;
        *)
            if [ "$per_process_maximum" -gt 4096 ]; then
                candidates="$desired_soft_limit $per_process_maximum 4096"
            else
                candidates="$desired_soft_limit 4096 $per_process_maximum"
            fi
            ;;
    esac
    # The desired limit comes first and every later candidate has to be
    # smaller than the one before, so none exceeds the desired limit.
    previous_candidate=
    for candidate in $candidates; do
        if [ -n "$previous_candidate" ] && [ "$candidate" -ge "$previous_candidate" ]; then
            continue
        fi
        previous_candidate=$candidate
        if [ "$current_soft_limit" -ge "$candidate" ]; then
            return 0
        fi
        if ulimit -S -n "$candidate" 2>/dev/null; then
            return 0
        fi
    done
    return 1
}
