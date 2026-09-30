//! Rule decks (Phase 5b SLICE 2): the blanket placeholder and a real PDK table.
//!
//! A deck is data — per (layer, datatype) min width + min space — and the
//! engine never special-cases a PDK. Two properties matter as much as the
//! numbers:
//!
//! * **Datatype filtering.** A deck lists *drawing* purposes only. Pins (x/16),
//!   labels (x/5), boundary and marker layers carry no geometry rules, and the
//!   blanket deck checking them produced flags no signoff tool would.
//! * **Named rules.** Each check carries its PDK rule id (`m1.1`, `m1.2`, …) so
//!   a violation reads the way the design manual does.

use super::{DrcRules, LayerRule};
use crate::types::Layer;
use std::collections::HashMap;

/// One deck row: a drawing layer and its width / space rules (nm).
#[derive(Debug, Clone, Copy)]
pub struct DeckEntry {
    pub layer: Layer,
    pub name: &'static str,
    pub width_rule: &'static str,
    pub min_width: i32,
    pub space_rule: &'static str,
    pub min_spacing: i32,
}

const fn e(
    l: u16,
    d: u16,
    name: &'static str,
    width_rule: &'static str,
    min_width: i32,
    space_rule: &'static str,
    min_spacing: i32,
) -> DeckEntry {
    DeckEntry { layer: Layer { layer: l, datatype: d }, name, width_rule, min_width, space_rule, min_spacing }
}

/// SkyWater SKY130 — min width / min space for the drawing layers, from the
/// periphery rules of the SKY130 design manual (rule ids as published).
///
/// Deliberately a subset, and scoped:
/// * only the basic same-layer width/space rules — no wide-metal spacing
///   (`m1.3b`…), end-of-line, enclosure, extension, area, density or exact-via
///   size rules;
/// * `dnwell` is omitted: its 6.3 µm spacing would widen every tile's reach
///   (the grid bucketing halo is the deck's max reach) for one rarely-edited
///   layer;
/// * intra-cell like everything else (see the module note in `drc`).
pub const SKY130: &[DeckEntry] = &[
    e(64, 20, "nwell", "nwell.1", 840, "nwell.2a", 1270),
    e(65, 20, "diff", "difftap.1", 150, "difftap.3", 270),
    e(65, 44, "tap", "difftap.1", 150, "difftap.3", 270),
    e(66, 20, "poly", "poly.1a", 150, "poly.2", 210),
    e(66, 44, "licon1", "licon.1", 170, "licon.2", 170),
    e(67, 20, "li1", "li.1", 170, "li.3", 170),
    e(67, 44, "mcon", "ct.1", 170, "ct.2", 190),
    e(68, 20, "met1", "m1.1", 140, "m1.2", 140),
    e(68, 44, "via", "via.1a", 150, "via.2", 170),
    e(69, 20, "met2", "m2.1", 140, "m2.2", 140),
    e(69, 44, "via2", "via2.1a", 200, "via2.2", 200),
    e(70, 20, "met3", "m3.1", 300, "m3.2", 300),
    e(70, 44, "via3", "via3.1", 200, "via3.2", 200),
    e(71, 20, "met4", "m4.1", 300, "m4.2", 300),
    e(71, 44, "via4", "via4.1", 800, "via4.2", 800),
    e(72, 20, "met5", "m5.1", 1600, "m5.2", 1600),
    e(75, 20, "hvi", "hvi.1", 600, "hvi.2a", 700),
    e(78, 44, "hvtp", "hvtp.1", 380, "hvtp.2", 380),
    e(93, 44, "nsdm", "nsd.1", 380, "nsd.2", 380),
    e(94, 20, "psdm", "psd.1", 380, "psd.2", 380),
    e(95, 20, "npc", "npc.1", 270, "npc.2", 270),
    e(125, 44, "lvtn", "lvtn.1a", 380, "lvtn.2", 380),
];

/// The rule decks the app offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deck {
    /// SKY130 drawing-layer width/space ([`SKY130`]).
    Sky130,
    /// 100 nm / 100 nm on every layer and datatype — a placeholder that makes a
    /// careless drawing flag at once on any design.
    Blanket,
}

impl Deck {
    pub const ALL: [Deck; 2] = [Deck::Sky130, Deck::Blanket];

    pub fn label(self) -> &'static str {
        match self {
            Deck::Sky130 => "sky130 (drawing layers)",
            Deck::Blanket => "blanket 100/100 nm",
        }
    }

    pub fn rules(self) -> DrcRules {
        match self {
            Deck::Sky130 => DrcRules::from_entries(SKY130),
            Deck::Blanket => DrcRules::default_table(),
        }
    }

    /// Named-rule lookup for a violation on `layer` (`None` for the blanket deck
    /// or an unlisted layer).
    pub fn entry(self, layer: Layer) -> Option<&'static DeckEntry> {
        match self {
            Deck::Sky130 => SKY130.iter().find(|e| e.layer == layer),
            Deck::Blanket => None,
        }
    }

    /// Pick a deck for a design from its layer set: SKY130 when the core
    /// back-end stack (li1, met1, met2 drawing) is present, else the blanket.
    pub fn detect(layers: &[Layer]) -> Deck {
        let has = |l, d| layers.contains(&Layer::new(l, d));
        if has(67, 20) && has(68, 20) && has(69, 20) {
            Deck::Sky130
        } else {
            Deck::Blanket
        }
    }

    /// Parse a deck name (`sky130` / `blanket`) — the test hook and CLI.
    pub fn from_name(name: &str) -> Option<Deck> {
        match name {
            "sky130" => Some(Deck::Sky130),
            "blanket" => Some(Deck::Blanket),
            _ => None,
        }
    }
}

impl DrcRules {
    /// A per-layer deck (no blanket default: unlisted layers/datatypes are not
    /// checked).
    pub fn from_entries(entries: &[DeckEntry]) -> Self {
        let per_layer: HashMap<Layer, LayerRule> = entries
            .iter()
            .map(|e| (e.layer, LayerRule { min_width: e.min_width, min_spacing: e.min_spacing }))
            .collect();
        DrcRules { default_rule: None, per_layer }
    }

    /// The SKY130 deck (see [`SKY130`]).
    pub fn sky130() -> Self {
        DrcRules::from_entries(SKY130)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sky130_checks_drawing_purposes_only() {
        let r = DrcRules::sky130();
        assert_eq!(r.rule_for(Layer::new(68, 20)), Some(LayerRule { min_width: 140, min_spacing: 140 }));
        assert!(r.rule_for(Layer::new(68, 16)).is_none(), "met1 pin purpose is not checked");
        assert!(r.rule_for(Layer::new(68, 5)).is_none(), "met1 label purpose is not checked");
        assert!(r.rule_for(Layer::new(235, 4)).is_none(), "unlisted layers are not checked");
        assert_eq!(r.max_reach(), 1600 + 2, "met5 space sets the reach");
    }

    #[test]
    fn deck_rows_are_unique_and_sane() {
        for (i, a) in SKY130.iter().enumerate() {
            assert!(a.min_width > 0 && a.min_spacing > 0, "{}", a.name);
            assert!(SKY130[i + 1..].iter().all(|b| b.layer != a.layer), "duplicate row for {}", a.name);
        }
    }

    #[test]
    fn detect_picks_sky130_by_its_backend_stack() {
        let sky = [Layer::new(67, 20), Layer::new(68, 20), Layer::new(69, 20), Layer::new(68, 16)];
        assert_eq!(Deck::detect(&sky), Deck::Sky130);
        assert_eq!(Deck::detect(&[Layer::new(1, 0), Layer::new(2, 0)]), Deck::Blanket);
    }
}
