/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Policy for the `--fix-texture-min-filter` compatibility workaround.
//!
//! # The problem
//!
//! The default `GL_TEXTURE_MIN_FILTER` is `GL_NEAREST_MIPMAP_LINEAR`, so a
//! texture that only ever had level 0 uploaded is mipmap-incomplete until the
//! app sets a non-mipmap filter. The OpenGL ES specification says an
//! incomplete texture samples as (0, 0, 0, 1), and that is also what the
//! OpenGL ES 2.0 drivers on iOS and ANGLE do. Some strict *native OpenGL ES
//! 1.1* Android drivers (Mali r32p1, Adreno) do the same, which turns iOS-era
//! ES 1.1 games that relied on lenient PowerVR behaviour into flat black
//! quads.
//!
//! # What this module does about it
//!
//! * The guest's requested filter is **recorded per texture**, and
//!   `glGetTexParameter*` keeps returning it: the guest-visible state is never
//!   altered, only what the host driver is given.
//! * The host is given the guest's filter unchanged whenever the texture is
//!   mipmap-complete enough to use it, i.e. once any level above 0 has been
//!   uploaded or `glGenerateMipmap` has been called. Legitimate mipmapping is
//!   therefore preserved.
//! * Only a texture with *no* mip levels, whose guest filter is a mipmap
//!   filter, gets the nearest non-mipmap equivalent. A filter the guest set to
//!   a non-mipmap mode is never touched, and uploading level 0 again never
//!   overwrites a filter the guest chose.
//! * A texture whose level 0 is not power-of-two keeps the non-mipmap
//!   equivalent even with mip levels: mipmapped NPOT textures are incomplete
//!   on ES 1.1 / ES 2.0 core, and `GLES1Native` applies the same rule at
//!   upload time, which this must not undo.
//! * The workaround is for native ES 1.1 contexts only. ES 2.0 contexts get
//!   spec-conformant behaviour from ANGLE, matching iOS, so substituting there
//!   would hide real guest bugs and degrade mipmapped textures for nothing.
//!
//! "Has any level above 0" approximates mipmap completeness (a chain with
//! gaps is still incomplete); an app with a broken chain gets the same black
//! result it would on iOS.
//!
//! Texture names are tracked per EAGLContext. A texture shared between
//! contexts of one sharegroup is not tracked across them, so mip levels
//! uploaded from another context are not seen.

use crate::gles::gles11_raw as gles11;
use std::collections::HashMap;
use touchHLE_gl_bindings::gles11::types::{GLenum, GLint, GLsizei, GLuint};

/// The OpenGL ES default for `GL_TEXTURE_MIN_FILTER`.
pub(super) const DEFAULT_MIN_FILTER: GLenum = gles11::NEAREST_MIPMAP_LINEAR;

/// Whether the workaround may act at all: the option must be on and the
/// context must be a native OpenGL ES 1.x one.
pub(super) fn workaround_applies(option_enabled: bool, context_is_es1: bool) -> bool {
    option_enabled && context_is_es1
}

/// The closest non-mipmap filter to `filter`; non-mipmap filters and unknown
/// values are returned unchanged.
pub(super) fn without_mipmaps(filter: GLenum) -> GLenum {
    match filter {
        gles11::NEAREST_MIPMAP_NEAREST | gles11::NEAREST_MIPMAP_LINEAR => gles11::NEAREST,
        gles11::LINEAR_MIPMAP_NEAREST | gles11::LINEAR_MIPMAP_LINEAR => gles11::LINEAR,
        other => other,
    }
}

#[derive(Clone, Copy)]
struct Entry {
    /// What the guest asked for (or the GL default).
    guest: GLenum,
    /// Whether the guest ever set the filter itself.
    guest_set: bool,
    /// Whether any level above 0 exists, or mipmaps were generated.
    has_mip_levels: bool,
    /// Whether level 0 has non-power-of-two dimensions.
    npot_level0: bool,
}

const UNTOUCHED: Entry = Entry {
    guest: DEFAULT_MIN_FILTER,
    guest_set: false,
    has_mip_levels: false,
    npot_level0: false,
};

fn is_power_of_two(n: GLsizei) -> bool {
    n > 0 && n & (n - 1) == 0
}

/// Per-texture record of the guest's min filter and mip-level presence.
#[derive(Default)]
pub(super) struct MinFilterTracker {
    textures: HashMap<GLuint, Entry>,
}

impl MinFilterTracker {
    fn entry(&mut self, texture: GLuint) -> &mut Entry {
        self.textures.entry(texture).or_insert(UNTOUCHED)
    }

    fn effective_of(entry: Entry) -> GLenum {
        if entry.has_mip_levels && !entry.npot_level0 {
            entry.guest
        } else if !entry.guest_set {
            // Never set by the guest: the default would leave a level-0-only
            // texture incomplete, so use plain linear filtering, which is
            // what lenient ES 1.1 drivers effectively gave these apps.
            gles11::LINEAR
        } else {
            without_mipmaps(entry.guest)
        }
    }

    /// What the guest believes the filter is (for `glGetTexParameter*`).
    pub(super) fn guest_filter(&self, texture: GLuint) -> GLenum {
        self.textures
            .get(&texture)
            .map_or(DEFAULT_MIN_FILTER, |entry| entry.guest)
    }

    /// What the host driver should be given for `texture` right now.
    #[cfg(test)]
    pub(super) fn effective_filter(&self, texture: GLuint) -> GLenum {
        Self::effective_of(self.textures.get(&texture).copied().unwrap_or(UNTOUCHED))
    }

    /// The guest set `GL_TEXTURE_MIN_FILTER`. Returns the host value.
    pub(super) fn set_guest_filter(&mut self, texture: GLuint, filter: GLenum) -> GLenum {
        let entry = self.entry(texture);
        entry.guest = filter;
        entry.guest_set = true;
        Self::effective_of(*entry)
    }

    /// An image was specified for `level` with the given size. Returns the
    /// host value.
    pub(super) fn note_level_uploaded(
        &mut self,
        texture: GLuint,
        level: GLint,
        width: GLsizei,
        height: GLsizei,
    ) -> GLenum {
        let entry = self.entry(texture);
        if level > 0 {
            entry.has_mip_levels = true;
        } else if level == 0 {
            entry.npot_level0 = !(is_power_of_two(width) && is_power_of_two(height));
        }
        Self::effective_of(*entry)
    }

    /// `glGenerateMipmap` ran. Returns the host value.
    pub(super) fn note_mipmaps_generated(&mut self, texture: GLuint) -> GLenum {
        let entry = self.entry(texture);
        entry.has_mip_levels = true;
        Self::effective_of(*entry)
    }

    /// The texture was deleted.
    pub(super) fn forget(&mut self, texture: GLuint) {
        self.textures.remove(&texture);
    }
}

/// `GLint` form of a filter, for the driver calls.
pub(super) fn as_param(filter: GLenum) -> GLint {
    filter as GLint
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: GLuint = 5;

    /// Upload `level` of a 64x64-based (power-of-two) texture.
    fn up(t: &mut MinFilterTracker, level: GLint) -> GLenum {
        t.note_level_uploaded(T, level, 64 >> level, 64 >> level)
    }

    #[test]
    fn es1_default_filter_texture_with_only_level_zero_is_sampled_linearly() {
        // The case the workaround exists for: a game that never sets a filter.
        let mut t = MinFilterTracker::default();
        assert_eq!(up(&mut t, 0), gles11::LINEAR);
        // The guest still sees the real default.
        assert_eq!(t.guest_filter(T), gles11::NEAREST_MIPMAP_LINEAR);
    }

    #[test]
    fn explicit_mipmap_filter_without_mip_levels_is_demipmapped_but_remembered() {
        let mut t = MinFilterTracker::default();
        up(&mut t, 0);
        assert_eq!(
            t.set_guest_filter(T, gles11::LINEAR_MIPMAP_LINEAR),
            gles11::LINEAR
        );
        assert_eq!(t.guest_filter(T), gles11::LINEAR_MIPMAP_LINEAR);
        assert_eq!(
            t.set_guest_filter(T, gles11::NEAREST_MIPMAP_NEAREST),
            gles11::NEAREST
        );
        assert_eq!(t.guest_filter(T), gles11::NEAREST_MIPMAP_NEAREST);
    }

    #[test]
    fn uploading_mip_levels_restores_the_guests_mipmap_filter() {
        // Legitimate mipmapping must survive: filter set first, chain later.
        let mut t = MinFilterTracker::default();
        up(&mut t, 0);
        t.set_guest_filter(T, gles11::LINEAR_MIPMAP_LINEAR);
        assert_eq!(up(&mut t, 1), gles11::LINEAR_MIPMAP_LINEAR);
        assert_eq!(t.effective_filter(T), gles11::LINEAR_MIPMAP_LINEAR);
    }

    #[test]
    fn mip_levels_first_then_filter_passes_through() {
        // Chain first, filter later (the other common order).
        let mut t = MinFilterTracker::default();
        up(&mut t, 0);
        up(&mut t, 1);
        up(&mut t, 2);
        assert_eq!(
            t.set_guest_filter(T, gles11::NEAREST_MIPMAP_LINEAR),
            gles11::NEAREST_MIPMAP_LINEAR
        );
    }

    #[test]
    fn generate_mipmap_counts_as_a_mip_chain() {
        let mut t = MinFilterTracker::default();
        up(&mut t, 0);
        t.set_guest_filter(T, gles11::LINEAR_MIPMAP_NEAREST);
        assert_eq!(t.effective_filter(T), gles11::LINEAR);
        assert_eq!(t.note_mipmaps_generated(T), gles11::LINEAR_MIPMAP_NEAREST);
    }

    #[test]
    fn npot_textures_stay_demipmapped_even_with_mip_levels() {
        // Matches GLES1Native's NPOT rule; a later level upload must not undo it.
        let mut t = MinFilterTracker::default();
        t.note_level_uploaded(T, 0, 100, 60);
        t.set_guest_filter(T, gles11::LINEAR_MIPMAP_LINEAR);
        assert_eq!(t.note_level_uploaded(T, 1, 50, 30), gles11::LINEAR);
        assert_eq!(t.note_mipmaps_generated(T), gles11::LINEAR);
        assert_eq!(t.guest_filter(T), gles11::LINEAR_MIPMAP_LINEAR);
        // Re-specifying level 0 as power-of-two lifts the restriction.
        assert_eq!(
            t.note_level_uploaded(T, 0, 64, 64),
            gles11::LINEAR_MIPMAP_LINEAR
        );
    }

    #[test]
    fn non_mipmap_filters_are_never_touched() {
        let mut t = MinFilterTracker::default();
        for filter in [gles11::NEAREST, gles11::LINEAR] {
            assert_eq!(t.set_guest_filter(T, filter), filter);
            assert_eq!(t.guest_filter(T), filter);
        }
    }

    #[test]
    fn reuploading_level_zero_does_not_clobber_a_guest_chosen_filter() {
        let mut t = MinFilterTracker::default();
        t.set_guest_filter(T, gles11::NEAREST);
        // Texture re-specified (e.g. a dynamic texture): filter is unchanged.
        assert_eq!(up(&mut t, 0), gles11::NEAREST);
        assert_eq!(t.guest_filter(T), gles11::NEAREST);
    }

    #[test]
    fn textures_are_independent_and_deletion_resets_state() {
        let mut t = MinFilterTracker::default();
        t.set_guest_filter(1, gles11::LINEAR_MIPMAP_LINEAR);
        t.note_level_uploaded(1, 1, 32, 32);
        assert_eq!(t.effective_filter(2), gles11::LINEAR); // untouched default
        t.forget(1);
        assert_eq!(t.guest_filter(1), gles11::NEAREST_MIPMAP_LINEAR);
        assert_eq!(t.effective_filter(1), gles11::LINEAR);
    }

    #[test]
    fn the_workaround_only_applies_to_es1_contexts_with_the_option_on() {
        // ES 1.1 sequence: workaround active.
        assert!(workaround_applies(true, true));
        // ES 2.0 sequence: spec-conformant, no substitution.
        assert!(!workaround_applies(true, false));
        // Option off (e.g. --no-fix-texture-min-filter): nothing happens.
        assert!(!workaround_applies(false, true));
        assert!(!workaround_applies(false, false));
    }

    #[test]
    fn without_mipmaps_maps_every_mipmap_filter() {
        assert_eq!(without_mipmaps(gles11::NEAREST_MIPMAP_NEAREST), gles11::NEAREST);
        assert_eq!(without_mipmaps(gles11::NEAREST_MIPMAP_LINEAR), gles11::NEAREST);
        assert_eq!(without_mipmaps(gles11::LINEAR_MIPMAP_NEAREST), gles11::LINEAR);
        assert_eq!(without_mipmaps(gles11::LINEAR_MIPMAP_LINEAR), gles11::LINEAR);
        assert_eq!(without_mipmaps(gles11::NEAREST), gles11::NEAREST);
        assert_eq!(without_mipmaps(gles11::LINEAR), gles11::LINEAR);
        assert_eq!(without_mipmaps(0x1234), 0x1234);
    }
}
