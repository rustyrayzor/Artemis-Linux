# Artemis HDR reference session

These files install a dedicated Weston 15.0.1 kiosk session for native HDR10 on
UbuntuLab. They do not replace GNOME or Ubuntu's compositor packages.

- `weston.ini` is calibrated to the reference Hisense on `card1-HDMI-A-1`.
- `artemis-hdr-session` starts the side-by-side compositor in `/opt/artemis-hdr`.
- `artemis-hdr-launch` starts the installed release and tells Artemis that
  Weston owns vblank pacing.
- `artemis-hdr.desktop` exposes the session to GDM.
- `accountsservice-ray-svc` and `gdm-custom.conf` are reference-host templates;
  back up the live files before installing them.
- `artemis-hdr-launch-validation` is a bounded local Apollo smoke test and must
  not be installed as the normal launcher.

Apply `../weston-patches/0001-parametric-stock-srgb.patch` to Weston 15.0.1
before building. See `docs/ubuntu-reference-runbook.md` for installation,
validation, and rollback steps.
