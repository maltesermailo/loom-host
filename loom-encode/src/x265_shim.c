/* Stable-symbol shim for x265's build-number-versioned open macro.
 * `x265_encoder_open` expands to `x265_encoder_open_<X265_BUILD>`, which has no
 * fixed name to bind from Rust; this wrapper gives one. */
#include <x265.h>

x265_encoder *loom_x265_encoder_open(x265_param *param) {
  return x265_encoder_open(param);
}
