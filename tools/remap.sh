# Sourced by the build scripts. The compiler writes the path of every
# source file that can report an error into the binary; left alone, those
# are the build machine's own folders (its home folder, and so its user
# name). remap_paths <target> prints the cargo option that rewrites them:
# the checkout to /swarm, cargo's downloads to /cargo, anything else under
# the home folder to /home. Cargo adds it to that target's own rustflags
# (.cargo/config.toml, CARGO_TARGET_*_RUSTFLAGS) rather than replacing them.
remap_paths() {
  local top cargo_home
  top=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
  cargo_home=${CARGO_HOME:-$HOME/.cargo}
  # When several match, rustc uses the last one: the general one first.
  printf -- '--config=target.%s.rustflags=["--remap-path-prefix=%s=/home","--remap-path-prefix=%s=/cargo","--remap-path-prefix=%s=/swarm"]' \
    "$1" "$HOME" "$cargo_home" "$top"
}
