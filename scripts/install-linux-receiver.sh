#!/bin/sh
# Installs the Linux receiver as a systemd user service:
#
#   binary  ~/.local/bin/edge-receiver-linux
#   config  ~/.config/edge-kvm/receiver.toml
#   state   ~/.local/state/edge-kvm  (identity, pins, logs)
#
# Build first with `cargo build --release -p edge-receiver-linux`, then run
# this from the checkout; run it again after each build to update.
#
# The first run copies an existing portable setup (receiver.toml and state/
# next to the binary) so the identity and pins carry over; the originals are
# left alone. Pass the folder holding them if it isn't this checkout's
# target/release.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
build="$repo/target/release"
from="${1:-$build}"
bin_dir="$HOME/.local/bin"
config_dir="$HOME/.config/edge-kvm"
state_dir="$HOME/.local/state/edge-kvm"
unit_dir="$HOME/.config/systemd/user"
unit="edge-kvm-receiver.service"

if [ ! -x "$build/edge-receiver-linux" ]; then
  echo "No release build; run: cargo build --release -p edge-receiver-linux" >&2
  exit 1
fi

mkdir -p "$bin_dir" "$config_dir" "$unit_dir"
mkdir -p "$state_dir"

if [ ! -e "$config_dir/receiver.toml" ]; then
  if [ -f "$from/receiver.toml" ]; then
    cp -p "$from/receiver.toml" "$config_dir/receiver.toml"
    echo "Copied the config from $from"
  else
    cp "$repo/config/receiver.example.toml" "$config_dir/receiver.toml"
    echo "Created $config_dir/receiver.toml from the example; check it before relying on it"
  fi
fi

if [ ! -e "$state_dir/identity.toml" ] && [ -d "$from/state" ]; then
  cp -Rp "$from/state/." "$state_dir/"
  echo "Copied the identity and pins from $from/state"
fi
# The state holds the private key: owner only, even if the copy said otherwise.
chmod 700 "$state_dir"
if [ -f "$state_dir/identity.toml" ]; then
  chmod 600 "$state_dir/identity.toml"
fi

# Replace rather than overwrite, so a running receiver keeps its binary.
cp "$build/edge-receiver-linux" "$bin_dir/.edge-receiver-linux.new"
mv -f "$bin_dir/.edge-receiver-linux.new" "$bin_dir/edge-receiver-linux"

if [ -f "$unit_dir/$unit" ] && ! cmp -s "$repo/packaging/linux/$unit" "$unit_dir/$unit"; then
  cp -p "$unit_dir/$unit" "$unit_dir/$unit.bak"
  echo "Saved the previous unit as $unit_dir/$unit.bak"
fi
cp "$repo/packaging/linux/$unit" "$unit_dir/$unit"

for dropin in "$unit_dir/$unit.d"/*.conf; do
  if [ -f "$dropin" ]; then
    echo "Note: $dropin still applies and may override this install"
  fi
done

systemctl --user daemon-reload
systemctl --user enable "$unit" >/dev/null
systemctl --user restart "$unit"
echo "edge-kvm receiver installed and running ($(systemctl --user is-active "$unit"))"
