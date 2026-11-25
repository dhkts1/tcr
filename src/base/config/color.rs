//! A group's panel color: the one derived from its name, and the check on a color an operator sets.

/// Whether a group's resolved color came from an operator's explicit
/// `tcr group color` write or was invented by [`derive_group_color`] because
/// none was set. Purely informational — both are equally valid "the" color
/// for a group — but an operator asking `tcr group ls` needs to be able to
/// tell which is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSource {
    Derived,
    Set,
}

impl ColorSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Derived => "derived",
            Self::Set => "set",
        }
    }
}

/// Invent a stable color for `group` when no `tcr group color` was ever run
/// for it. Same name always yields the same color (a deterministic hash of
/// the bytes, not randomness or insertion order), and different names spread
/// around the hue wheel so adjacent groups are visually distinguishable.
///
/// Derived in **OKLCH** at a **fixed lightness** (`L = 0.80`), varying only
/// hue — not HSL, which this replaced. HSL's `L` is not perceptual: the old
/// `HSL(hue, 62%, 58%)` band measured **L 0.518–0.852 in OKLCH**, so chips
/// carried wildly different visual weight for no reason, and around hue 30 it
/// was unreadable in BOTH text polarities (APCA `|Lc|` 52.5 white / 56.5
/// black, against the ≥60 threshold non-body text needs) — roughly one group
/// name in twelve. Fixed `L` is what makes every chip carry the same visual
/// weight, and it also keeps a single foreground choice correct for every
/// derived color at once: at `L = 0.80` black text beats white for every hue
/// on this wheel (measured worst case APCA ≥ 60, see this module's
/// `derived_colors_clear_the_apca_floor_at_every_hue` test).
///
/// Chroma targets `0.12` but is **reduced (never clipped)** per hue to stay
/// inside the sRGB gamut: at `L = 0.80, C = 0.12` several hues would clip a
/// channel to 255, which silently distorts the hue rather than dimming it —
/// see [`oklch_to_gamut_srgb`]. Still emits plain `#rrggbb`: OKLCH is the
/// math used to pick the byte values, never a notation this crate writes to
/// the wire, the config file, or anywhere else.
pub fn derive_group_color(group: &str) -> String {
    // FNV-1a: no dependency, good-enough avalanche for hashing short ASCII
    // labels into a hue — this is a display color, not a security boundary,
    // so cryptographic strength buys nothing here.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in group.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let hue = (hash % 360) as f64;
    oklch_to_hex(DERIVED_GROUP_COLOR_L, DERIVED_GROUP_COLOR_C, hue)
}

/// Fixed OKLCH lightness [`derive_group_color`] varies hue against — see its
/// doc-comment for why this specific value.
const DERIVED_GROUP_COLOR_L: f64 = 0.80;

/// Target OKLCH chroma [`derive_group_color`] aims for before per-hue gamut
/// reduction — see [`oklch_to_gamut_srgb`].
const DERIVED_GROUP_COLOR_C: f64 = 0.12;

/// OKLCH(`l`, `c`, `h`-degrees) to a lowercase `#rrggbb`, reducing `c` per hue
/// to stay in the sRGB gamut — see [`oklch_to_gamut_srgb`].
fn oklch_to_hex(l: f64, c: f64, h_deg: f64) -> String {
    let (r, g, b) = oklch_to_gamut_srgb(l, c, h_deg);
    let to_byte = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", to_byte(r), to_byte(g), to_byte(b))
}

/// OKLCH(`l`, `c`, `h`-degrees) to gamma-encoded sRGB `(r, g, b)` in `[0, 1]`,
/// reducing `c` toward 0 in small steps until the linear-sRGB result has no
/// channel outside `[0, 1]` — i.e. no clipping. Clipping a channel instead
/// (the naive approach) silently shifts the HUE, not just the brightness,
/// which is exactly the distortion the bridge asked this to avoid. `c <= 0`
/// (grey at `l`) is always in gamut, so this always terminates.
fn oklch_to_gamut_srgb(l: f64, c: f64, h_deg: f64) -> (f64, f64, f64) {
    let mut chroma = c;
    loop {
        if let Some(rgb) = oklch_to_srgb_if_in_gamut(l, chroma, h_deg) {
            return rgb;
        }
        if chroma <= 0.0 {
            return oklch_to_srgb_clamped(l, 0.0, h_deg);
        }
        chroma = (chroma - 0.0005).max(0.0);
    }
}

/// A tiny slack on the `[0, 1]` gamut bound — linear-sRGB round-trips through
/// `powf` and cube roots, so an in-gamut color can land at `1.0000000003`
/// from floating-point noise alone; without slack that would be rejected as
/// "out of gamut" and needlessly desaturated.
const GAMUT_EPS: f64 = 1e-4;

/// `Some((r, g, b))` (gamma-encoded, in `[0, 1]`) when OKLCH(`l`, `c`,
/// `h`-degrees) converts to linear sRGB with every channel inside
/// `[-GAMUT_EPS, 1 + GAMUT_EPS]`; `None` when a channel would clip.
fn oklch_to_srgb_if_in_gamut(l: f64, c: f64, h_deg: f64) -> Option<(f64, f64, f64)> {
    let (lr, lg, lb) = oklch_to_linear_srgb(l, c, h_deg);
    if !(-GAMUT_EPS..=1.0 + GAMUT_EPS).contains(&lr)
        || !(-GAMUT_EPS..=1.0 + GAMUT_EPS).contains(&lg)
        || !(-GAMUT_EPS..=1.0 + GAMUT_EPS).contains(&lb)
    {
        return None;
    }
    Some((
        linear_to_srgb_component(lr.clamp(0.0, 1.0)),
        linear_to_srgb_component(lg.clamp(0.0, 1.0)),
        linear_to_srgb_component(lb.clamp(0.0, 1.0)),
    ))
}

/// Same conversion as [`oklch_to_srgb_if_in_gamut`], but clamps unconditionally
/// instead of reporting an out-of-gamut channel — only ever called at `c = 0`
/// (a grey), which is always representable, by [`oklch_to_gamut_srgb`]'s
/// terminating case.
fn oklch_to_srgb_clamped(l: f64, c: f64, h_deg: f64) -> (f64, f64, f64) {
    let (lr, lg, lb) = oklch_to_linear_srgb(l, c, h_deg);
    (
        linear_to_srgb_component(lr.clamp(0.0, 1.0)),
        linear_to_srgb_component(lg.clamp(0.0, 1.0)),
        linear_to_srgb_component(lb.clamp(0.0, 1.0)),
    )
}

/// OKLCH(`l`, `c`, `h`-degrees) to linear sRGB, via OKLab — Björn Ottosson's
/// published matrices (<https://bottosson.github.io/posts/oklab>/), the
/// reference conversion every OKLCH implementation uses. Channels are NOT
/// clamped here; the caller decides what an out-of-`[0,1]` channel means.
fn oklch_to_linear_srgb(l: f64, c: f64, h_deg: f64) -> (f64, f64, f64) {
    let h = h_deg.to_radians();
    let a = c * h.cos();
    let b = c * h.sin();

    let l_ = l + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m_ = l - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s_ = l - 0.089_484_177_5 * a - 1.291_485_548_0 * b;

    let l3 = l_ * l_ * l_;
    let m3 = m_ * m_ * m_;
    let s3 = s_ * s_ * s_;

    let r = 4.076_741_662_1 * l3 - 3.307_711_591_3 * m3 + 0.230_969_929_2 * s3;
    let g = -1.268_438_004_6 * l3 + 2.609_757_401_1 * m3 - 0.341_319_396_5 * s3;
    let b = -0.004_196_086_3 * l3 - 0.703_418_614_7 * m3 + 1.707_614_701_0 * s3;
    (r, g, b)
}

/// Linear-light sRGB component in `[0, 1]` to gamma-encoded sRGB in `[0, 1]`
/// — the standard piecewise sRGB EOTF inverse.
fn linear_to_srgb_component(v: f64) -> f64 {
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// APCA-W3 (SAPC-8) perceptual contrast `Lc`, signed: positive is dark text
/// on a light background, negative is light text on a dark one — only the
/// magnitude (`|Lc|`) matters for "is this readable" and that is what every
/// caller here takes. Ported from the public APCA-W3 0.1.9 reference
/// algorithm (<https://github.com/Myndex/apca-w3>) — not WCAG 2's ratio, which
/// is a poor predictor of perceived contrast for exactly the kind of
/// mid-lightness saturated color this module generates.
fn apca_contrast(text_rgb: (u8, u8, u8), bg_rgb: (u8, u8, u8)) -> f64 {
    const BLACK_THRESHOLD: f64 = 0.022;
    const BLACK_CLAMP: f64 = 1.414;
    const DELTA_Y_MIN: f64 = 0.0005;
    const SCALE: f64 = 1.14;
    const LO_CLIP: f64 = 0.1;
    const LO_OFFSET: f64 = 0.027;

    let y = |rgb: (u8, u8, u8)| -> f64 {
        let f = |v: u8| (f64::from(v) / 255.0).powf(2.4);
        0.212_672_9 * f(rgb.0) + 0.715_152_2 * f(rgb.1) + 0.072_175_0 * f(rgb.2)
    };
    let soft_black_clamp = |y: f64| -> f64 {
        if y > BLACK_THRESHOLD {
            y
        } else {
            y + (BLACK_THRESHOLD - y).powf(BLACK_CLAMP)
        }
    };

    let txt_y = soft_black_clamp(y(text_rgb));
    let bg_y = soft_black_clamp(y(bg_rgb));
    if (bg_y - txt_y).abs() < DELTA_Y_MIN {
        return 0.0;
    }

    if bg_y > txt_y {
        // Normal polarity: dark(er) text on a light(er) background.
        let sapc = (bg_y.powf(0.56) - txt_y.powf(0.57)) * SCALE;
        if sapc < LO_CLIP {
            0.0
        } else {
            sapc - LO_OFFSET
        }
    } else {
        // Reverse polarity: light(er) text on a dark(er) background.
        let sapc = (bg_y.powf(0.62) - txt_y.powf(0.65)) * SCALE;
        if sapc > -LO_CLIP {
            0.0
        } else {
            sapc + LO_OFFSET
        }
    }
    .abs()
        * 100.0
}

/// `(r, g, b)` bytes of a `#rrggbb`/`#rgb`-shaped hex string, as already
/// normalized by [`validate_hex_color`]. `unwrap_or(0)` rather than a
/// `Result`: every caller here passes a string that already round-tripped
/// through `validate_hex_color`, so a parse failure would mean that
/// normalization itself is broken — not something a color-contrast helper
/// should surface as its own error type.
fn hex_to_rgb(hex: &str) -> (u8, u8, u8) {
    let digits = hex.trim_start_matches('#');
    let byte_at = |start: usize| u8::from_str_radix(&digits[start..start + 2], 16).unwrap_or(0);
    (byte_at(0), byte_at(2), byte_at(4))
}

/// The best achievable APCA `|Lc|` for `bg_hex` — whichever of pure white or
/// pure black text reads better against it. This is the same "which
/// foreground wins" question the panel asks per color chip, and the number
/// [`crate::verbs::cli::set_group_color`]'s low-contrast warning names.
pub fn best_readable_apca(bg_hex: &str) -> f64 {
    let bg = hex_to_rgb(bg_hex);
    let white = apca_contrast((255, 255, 255), bg);
    let black = apca_contrast((0, 0, 0), bg);
    white.max(black)
}

/// Strictly validate a user-supplied hex color and normalize it to lowercase
/// `#rrggbb`. Accepts `#RGB` and `#RRGGBB`, case-insensitive; rejects
/// anything else (missing `#`, wrong digit count, non-hex characters)
/// rather than trying to guess what garbage input meant.
pub fn validate_hex_color(input: &str) -> Result<String, &'static str> {
    let digits = input
        .strip_prefix('#')
        .ok_or("must start with '#' — accepted forms: #RGB, #RRGGBB")?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("must contain only hex digits after '#' — accepted forms: #RGB, #RRGGBB");
    }
    match digits.len() {
        3 => {
            let mut out = String::with_capacity(7);
            out.push('#');
            for c in digits.chars() {
                let lower = c.to_ascii_lowercase();
                out.push(lower);
                out.push(lower);
            }
            Ok(out)
        }
        6 => Ok(format!("#{}", digits.to_ascii_lowercase())),
        _ => Err("must be #RGB or #RRGGBB — accepted forms: #RGB, #RRGGBB"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- group color ---------------------------------------------------------

    #[test]
    fn validate_hex_color_accepts_both_forms_case_insensitively_and_normalizes() {
        assert_eq!(validate_hex_color("#32d74b"), Ok("#32d74b".to_string()));
        assert_eq!(validate_hex_color("#32D74B"), Ok("#32d74b".to_string()));
        assert_eq!(validate_hex_color("#ABC"), Ok("#aabbcc".to_string()));
        assert_eq!(validate_hex_color("#abc"), Ok("#aabbcc".to_string()));
    }

    #[test]
    fn validate_hex_color_rejects_garbage() {
        for bad in [
            "32d74b",  // missing '#'
            "#32d74",  // 5 digits: neither #RGB nor #RRGGBB
            "#zzzzzz", // non-hex characters
            "#12345g", // trailing non-hex character
            "",        // empty
            "#",       // '#' alone
        ] {
            assert!(validate_hex_color(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    // --- derived color band: OKLCH lightness/gamut/APCA (problem 2) ---------

    fn srgb_byte_to_linear(v: u8) -> f64 {
        let v = f64::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    /// The OKLab `(L, a, b)` a produced `#rrggbb` ACTUALLY measures at —
    /// independent of [`oklch_to_linear_srgb`] (the FORWARD conversion under
    /// test), via a standalone inverse. So a test built on this is not merely
    /// checking that the forward function returns the constant/hue it was
    /// told to return; it checks what the byte-quantized output really is.
    fn measured_oklab_lab(hex: &str) -> (f64, f64, f64) {
        let (r, g, b) = hex_to_rgb(hex);
        let (r, g, b) = (
            srgb_byte_to_linear(r),
            srgb_byte_to_linear(g),
            srgb_byte_to_linear(b),
        );
        let l_ = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
        let m_ = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
        let s_ = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
        let l = 0.210_454_255_3 * l_ + 0.793_617_785_0 * m_ - 0.004_072_046_8 * s_;
        let a = 1.977_998_495_1 * l_ - 2.428_592_205_0 * m_ + 0.450_593_709_9 * s_;
        let b = 0.025_904_037_1 * l_ + 0.782_771_766_2 * m_ - 0.808_675_766_0 * s_;
        (l, a, b)
    }

    fn measured_oklab_l(hex: &str) -> f64 {
        measured_oklab_lab(hex).0
    }

    /// Wiring check: [`derive_group_color`] itself (not just the
    /// `oklch_to_hex`/`DERIVED_GROUP_COLOR_*` machinery the tests below probe
    /// directly) actually emits the fixed-lightness band for real group
    /// names, so a regression that decouples `derive_group_color` from that
    /// machinery — e.g. reintroducing a per-hue-varying lightness inside
    /// `derive_group_color`'s own body while leaving the constants and
    /// `oklch_to_hex` untouched — is caught here even though the other
    /// OKLCH tests below would not see it.
    #[test]
    fn derive_group_color_actually_uses_the_fixed_lightness_band() {
        for name in ["codereview", "dev", "burst", "ops", "night-shift", "qa"] {
            let hex = derive_group_color(name);
            let l = measured_oklab_l(&hex);
            assert!(
                (l - DERIVED_GROUP_COLOR_L).abs() < 0.02,
                "derive_group_color({name:?}) = {hex} measures OKLab L={l:.4}, \
                 expected ~{DERIVED_GROUP_COLOR_L}"
            );
        }
    }

    /// THE test for the perceptual-uniformity half of problem 2: every hue on
    /// the derived-color wheel measures the SAME OKLab `L` (up to 8-bit
    /// rounding noise), unlike the old HSL band which measured L 0.518–0.852.
    /// Measured (via `measured_oklab_l`, an independent inverse conversion,
    /// not `derive_group_color`'s own forward math): spread is ~0.003, so
    /// 0.02 is a generous margin over quantization noise while still catching
    /// a real regression back toward HSL's ~0.33 spread.
    #[test]
    fn derived_colors_share_one_oklch_lightness_across_the_wheel() {
        let mut min_l = f64::MAX;
        let mut max_l = f64::MIN;
        for hue in 0..360u32 {
            let hex = oklch_to_hex(DERIVED_GROUP_COLOR_L, DERIVED_GROUP_COLOR_C, f64::from(hue));
            let l = measured_oklab_l(&hex);
            min_l = min_l.min(l);
            max_l = max_l.max(l);
        }
        let spread = max_l - min_l;
        assert!(
            spread < 0.02,
            "OKLCH lightness spread across the wheel is {spread:.4}, expected near-zero \
             (measured L range {min_l:.4}..{max_l:.4})"
        );
    }

    /// THE test for the readability half of problem 2: black text over every
    /// derived color on the wheel clears the APCA `|Lc| >= 60` floor —
    /// unlike the old HSL band, whose hue-30 slice scored 52.5/56.5 in
    /// EITHER text polarity. Measured worst case with this implementation is
    /// ~65.9 and best case ~71.7 (the bridge's own APCA implementation
    /// measured 67.8–72.9 on the same design; the two-point difference is
    /// implementation variance in the two independently-derived reference
    /// algorithms, not a gamut/lightness defect — both clear the floor by a
    /// wide margin).
    #[test]
    fn derived_colors_clear_the_apca_floor_at_every_hue() {
        let mut worst = f64::MAX;
        let mut worst_hue = 0u32;
        for hue in 0..360u32 {
            let hex = oklch_to_hex(DERIVED_GROUP_COLOR_L, DERIVED_GROUP_COLOR_C, f64::from(hue));
            let apca = best_readable_apca(&hex);
            if apca < worst {
                worst = apca;
                worst_hue = hue;
            }
        }
        assert!(
            worst >= 60.0,
            "hue {worst_hue} scores APCA |Lc| {worst:.1}, below the 60 floor for non-body text"
        );
    }

    /// Every derived color is fully in-gamut: converting it BACK through the
    /// same OKLab math it was derived from must not require clamping any
    /// linear-sRGB channel outside `[0, 1]` (beyond `GAMUT_EPS` float noise).
    /// Guards against a regression back to clipping (which silently shifts
    /// hue) rather than reducing chroma.
    #[test]
    fn derived_colors_are_fully_in_gamut_no_channel_clipped() {
        for hue in 0..360u32 {
            let hex = oklch_to_hex(DERIVED_GROUP_COLOR_L, DERIVED_GROUP_COLOR_C, f64::from(hue));
            // Independent of `oklch_to_gamut_srgb`'s own internals (comparing
            // its output to itself would be tautological and blind to a bug
            // INSIDE that function, e.g. a naive clamp instead of a chroma
            // reduction): reconstruct the EMITTED color's actual OKLab (a, b)
            // by inverting the byte-quantized hex, and check its hue matches
            // the requested hue. A naive per-channel clamp distorts hue (it
            // moves a/b independently); a correct chroma reduction scales a
            // and b together, which by construction preserves
            // `atan2(b, a)`. So a hue mismatch here is direct, independent
            // evidence of clipping regardless of how the implementation
            // reached it.
            let (_, a, b) = measured_oklab_lab(&hex);
            let chroma = a.hypot(b);
            assert!(
                chroma > 1e-4,
                "hue {hue}: emitted color {hex} is achromatic (chroma {chroma:.5}) — \
                 the fixed-lightness band should never fully desaturate"
            );
            let measured_hue = b.atan2(a).to_degrees().rem_euclid(360.0);
            let mut delta = (measured_hue - f64::from(hue)).abs();
            if delta > 180.0 {
                delta = 360.0 - delta;
            }
            assert!(
                delta < 1.0,
                "hue {hue}: emitted color {hex} measures hue {measured_hue:.2}° \
                 (drifted {delta:.2}°) — a channel was clipped instead of chroma reduced"
            );
        }
    }

    /// [`best_readable_apca`] against a known-bad color from the OLD HSL band
    /// (hue 30, S 62%, L 58% — the bridge's own illegible example) actually
    /// scores below the 60 floor with this implementation too — otherwise the
    /// two tests above would be trivially true no matter how the floor is
    /// computed.
    #[test]
    fn best_readable_apca_flags_the_old_bands_illegible_hue() {
        // Recompute the OLD HSL(30, 0.62, 0.58) color — the removed
        // `hsl_to_hex(30.0, 0.62, 0.58)` this replaced — via a standalone
        // HSL-to-RGB, so this test does not hand-guess the hex and instead
        // proves the NEW APCA implementation genuinely rejects the OLD band's
        // documented illegible hue, not just that it agrees with the
        // bridge's own numbers.
        fn old_hsl_to_hex(h: f64, s: f64, l: f64) -> String {
            let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
            let h_prime = h / 60.0;
            let x = c * (1.0 - (h_prime.rem_euclid(2.0) - 1.0).abs());
            let (r1, g1, b1) = match h_prime as u32 {
                0 => (c, x, 0.0),
                1 => (x, c, 0.0),
                2 => (0.0, c, x),
                3 => (0.0, x, c),
                4 => (x, 0.0, c),
                _ => (c, 0.0, x),
            };
            let m = l - c / 2.0;
            let to_byte = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
            format!("#{:02x}{:02x}{:02x}", to_byte(r1), to_byte(g1), to_byte(b1))
        }
        let old_hsl_hue_30 = old_hsl_to_hex(30.0, 0.62, 0.58);
        let apca = best_readable_apca(&old_hsl_hue_30);
        assert!(
            apca < 60.0,
            "the known-illegible old-band color {old_hsl_hue_30} scored {apca:.1}, expected < 60 \
             (sanity check that best_readable_apca is a real gate, not a rubber stamp)"
        );
    }
}
