# udev rule provenance

- `70-uaccess.rules` is copied from the `aclad` repository at revision
  `b4347297c895c47074555ea8eb0a9432e8a3cb0e`, with only the ownership comment
  updated. It is the direct-access tag policy now applied by `multiseatd`.
- `71-seat.rules` and `73-seat-late.rules` are copied verbatim from the
  `seat-udev` repository at commit
  `00126146f8e1ba1417ed4ec31541a079cff57f47`, matching the embedded
  `crates/seat-udev` source.

The `multiseat` package installs all three. The copy remains GPL-3.0-only;
license texts are in `crates/seat-udev/LICENSE` and the repository `LICENSE`.

`73-seat-late.rules` intentionally omits the systemd `uaccess` builtin because
integrated `multiseatd` is the sole direct-access ACL owner.
