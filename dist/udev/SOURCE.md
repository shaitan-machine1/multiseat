# udev rule provenance

These files are copied verbatim from the `seat-udev` repository at commit
`00126146f8e1ba1417ed4ec31541a079cff57f47`, matching the embedded
`crates/seat-udev` source.

They are checked in for future package ownership only. The current installed
rules and packages are unchanged; do not install these files over the active
rules until the reviewed deployment cutover. The copy remains GPL-3.0-only;
the license text is in `crates/seat-udev/LICENSE`.

`73-seat-late.rules` intentionally omits the systemd `uaccess` builtin because
integrated multiseatd becomes the sole direct-access ACL owner after cutover.
