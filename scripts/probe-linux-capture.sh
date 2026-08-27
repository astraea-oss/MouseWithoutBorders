#!/usr/bin/env bash
set -u

portal_service="org.freedesktop.portal.Desktop"
portal_path="/org/freedesktop/portal/desktop"
portal_interface="org.freedesktop.portal.InputCapture"

portal_ready=0
libei_ready=0
hyprland_ready=0

printf '%s\n' "Linux input-capture capability probe"
printf 'desktop=%s session=%s wayland_display=%s\n' \
  "${XDG_CURRENT_DESKTOP:-unknown}" \
  "${XDG_SESSION_TYPE:-unknown}" \
  "${WAYLAND_DISPLAY:-unset}"

if command -v busctl >/dev/null 2>&1; then
  portal_output="$(busctl --user introspect "$portal_service" "$portal_path" 2>&1)"
  if printf '%s\n' "$portal_output" | grep -Fq "$portal_interface"; then
    portal_ready=1
    printf '%s\n' "portal_input_capture=available"
    printf '%s\n' "$portal_output" | grep -F "$portal_interface"
  else
    printf '%s\n' "portal_input_capture=missing"
  fi
else
  printf '%s\n' "portal_input_capture=unknown (busctl is not installed)"
fi

if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists libei-1.0; then
  libei_ready=1
  printf 'libei=%s\n' "$(pkg-config --modversion libei-1.0)"
else
  printf '%s\n' "libei=missing (pkg-config could not resolve libei-1.0)"
fi

if command -v hyprctl >/dev/null 2>&1; then
  printf '%s\n' "hyprland_version_begin"
  hyprctl version 2>&1
  printf '%s\n' "hyprland_version_end"
else
  printf '%s\n' "hyprland=not-detected"
fi

if command -v wayland-info >/dev/null 2>&1; then
  wayland_output="$(wayland-info 2>&1)"
  if printf '%s\n' "$wayland_output" | grep -Eq 'hyprland_input_capture(_manager)?_v1'; then
    hyprland_ready=1
    printf '%s\n' "hyprland_input_capture=available"
    printf '%s\n' "$wayland_output" | grep -E 'hyprland_input_capture(_manager)?_v1'
  else
    printf '%s\n' "hyprland_input_capture=missing"
  fi
else
  printf '%s\n' "hyprland_input_capture=unknown (wayland-info is not installed)"
fi

if command -v pacman >/dev/null 2>&1; then
  printf '%s\n' "xdph_package_begin"
  pacman -Q xdg-desktop-portal xdg-desktop-portal-hyprland 2>&1 || true
  printf '%s\n' "xdph_package_end"
fi

if [ "$portal_ready" -eq 1 ] && [ "$libei_ready" -eq 1 ]; then
  printf '%s\n' "result=standard-portal-ready"
  exit 0
fi

if [ "$hyprland_ready" -eq 1 ]; then
  printf '%s\n' "result=hyprland-protocol-ready"
  exit 0
fi

printf '%s\n' "result=no-supported-capture-api"
printf '%s\n' "action=upgrade Hyprland/XDPH or install libei; raw evdev capture is not enabled implicitly"
exit 2
