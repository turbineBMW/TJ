//! While an agent works, its mark in the sidebar and the stop button breathe: they shrink for a
//! second and grow back for a second. Under the conversation, Claude Code's spinner verbs take
//! turns, with a wave running through each one's letters.
//!
//! Everything moves in steps on one shared clock, so it all changes on the same draw and one
//! timer drives it. The timer wakes the window [`STEPS_PER_SECOND`] times a second rather than
//! every frame: gpui draws the whole window each time, and a vsync-paced animation cost about
//! 40% of a core while an agent worked, against about 4% for this. When nothing is working
//! nothing asks for a wake-up, and the timer stops.

use std::{
    collections::HashMap,
    hash::BuildHasher,
    time::{Duration, Instant},
};

use gpui_kit::{App, Global, Task, Window, WindowId};

/// How often the window is drawn again while something works.
const STEPS_PER_SECOND: u32 = 12;
/// One breath, in steps: shrink for half of it, grow back for the other half.
const BREATH_STEPS: u64 = 2 * STEPS_PER_SECOND as u64;
/// The smallest a breathing mark gets, as a share of its size.
pub const SMALLEST: f32 = 0.7;
/// How many steps each spinner verb shows (three seconds).
const VERB_STEPS: u64 = 3 * STEPS_PER_SECOND as u64;
/// Steps of rest between one wave through a verb and the next.
const WAVE_REST: u64 = 6;

/// Claude Code's spinner verbs (its built-in list, as of 2.1.331).
pub const VERBS: &[&str] = &[
    "Accomplishing",
    "Actioning",
    "Actualizing",
    "Architecting",
    "Baking",
    "Beaming",
    "Beboppin'",
    "Befuddling",
    "Billowing",
    "Blanching",
    "Bloviating",
    "Boogieing",
    "Boondoggling",
    "Booping",
    "Bootstrapping",
    "Brewing",
    "Bunning",
    "Burrowing",
    "Calculating",
    "Caramelizing",
    "Cascading",
    "Catapulting",
    "Cerebrating",
    "Channeling",
    "Choreographing",
    "Churning",
    "Clauding",
    "Coalescing",
    "Cogitating",
    "Combobulating",
    "Composing",
    "Computing",
    "Concocting",
    "Considering",
    "Contemplating",
    "Cooking",
    "Crafting",
    "Creating",
    "Crunching",
    "Crystallizing",
    "Cultivating",
    "Deciphering",
    "Deliberating",
    "Determining",
    "Dilly-dallying",
    "Discombobulating",
    "Doing",
    "Doodling",
    "Drizzling",
    "Ebbing",
    "Effecting",
    "Elucidating",
    "Embellishing",
    "Enchanting",
    "Envisioning",
    "Fermenting",
    "Fiddle-faddling",
    "Finagling",
    "Flambéing",
    "Flibbertigibbeting",
    "Flowing",
    "Flummoxing",
    "Fluttering",
    "Forging",
    "Forming",
    "Frolicking",
    "Frosting",
    "Gallivanting",
    "Galloping",
    "Garnishing",
    "Generating",
    "Gesticulating",
    "Germinating",
    "Gitifying",
    "Grooving",
    "Gusting",
    "Harmonizing",
    "Hashing",
    "Hatching",
    "Herding",
    "Honking",
    "Hullaballooing",
    "Hyperspacing",
    "Ideating",
    "Imagining",
    "Improvising",
    "Incubating",
    "Inferring",
    "Infusing",
    "Ionizing",
    "Jitterbugging",
    "Julienning",
    "Kerfuffling",
    "Kneading",
    "Leavening",
    "Levitating",
    "Lollygagging",
    "Manifesting",
    "Marinating",
    "Meandering",
    "Metamorphosing",
    "Misting",
    "Moonwalking",
    "Moseying",
    "Mulling",
    "Mustering",
    "Nebulizing",
    "Nesting",
    "Newspapering",
    "Noodling",
    "Nucleating",
    "Onioning",
    "Orbiting",
    "Orchestrating",
    "Osmosing",
    "Perambulating",
    "Percolating",
    "Perusing",
    "Philosophizing",
    "Photosynthesizing",
    "Polishing",
    "Pollinating",
    "Pondering",
    "Pontificating",
    "Pouncing",
    "Precipitating",
    "Prestidigitating",
    "Processing",
    "Proofing",
    "Propagating",
    "Puttering",
    "Puzzling",
    "Quantumizing",
    "Razzle-dazzling",
    "Razzmatazzing",
    "Recombobulating",
    "Reticulating",
    "Roosting",
    "Ruminating",
    "Sautéing",
    "Scampering",
    "Schlepping",
    "Scurrying",
    "Seasoning",
    "Shenaniganing",
    "Shimmying",
    "Simmering",
    "Skedaddling",
    "Sketching",
    "Slithering",
    "Smooshing",
    "Sock-hopping",
    "Spelunking",
    "Spinning",
    "Sprouting",
    "Stewing",
    "Sublimating",
    "Swirling",
    "Swooping",
    "Symbioting",
    "Synthesizing",
    "Tempering",
    "Thinking",
    "Thundering",
    "Tinkering",
    "Tomfoolering",
    "Topsy-turvying",
    "Transfiguring",
    "Transmogrifying",
    "Transmuting",
    "Twisting",
    "Undulating",
    "Unfurling",
    "Unraveling",
    "Vibing",
    "Waddling",
    "Wandering",
    "Warping",
    "Whatchamacalliting",
    "Whirlpooling",
    "Whirring",
    "Whisking",
    "Wibbling",
    "Working",
    "Wrangling",
    "Zesting",
    "Zigzagging",
];

/// The shared clock, and each window's next wake-up.
struct Beat {
    epoch: Instant,
    wakes: HashMap<WindowId, (Instant, Task<()>)>,
}

impl Global for Beat {}

impl Default for Beat {
    fn default() -> Self {
        Self { epoch: Instant::now(), wakes: HashMap::new() }
    }
}

/// The step `since` the clock started falls in, and how long until the next one.
fn step(since: Duration) -> (u64, Duration) {
    let every = Duration::from_secs(1) / STEPS_PER_SECOND;
    let n = (since.as_nanos() / every.as_nanos()) as u64;
    (n, every * (n + 1) as u32 - since)
}

/// The step this frame is drawn at. Call it only while something works: it keeps the window
/// drawing a step at a time until a frame goes by without it.
pub fn tick(window: &mut Window, cx: &mut App) -> u64 {
    let now = cx.try_global::<crate::caret::Clock>().and_then(|c| c.now).unwrap_or_else(Instant::now);
    let beat = cx.default_global::<Beat>();
    let (n, wait) = step(now.saturating_duration_since(beat.epoch));
    let id = window.window_handle().window_id();
    // The next step's start, exactly: every draw inside one step shares its wake-up.
    let at = now + wait;
    if beat.wakes.get(&id).is_none_or(|(when, _)| *when != at) {
        let handle = window.window_handle();
        let task = cx.spawn(async move |cx| {
            cx.background_executor().timer(wait).await;
            let _ = handle.update(cx, |_, window, _| window.refresh());
        });
        cx.global_mut::<Beat>().wakes.insert(id, (at, task));
    }
    n
}

/// How big a breathing mark is at `step`: 1 at the top of a breath, eased down to
/// [`SMALLEST`] halfway and back, on a cosine so it slows at each end.
pub fn breath(step: u64) -> f32 {
    let phase = (step % BREATH_STEPS) as f32 / BREATH_STEPS as f32;
    let depth = (1.0 - (phase * std::f32::consts::TAU).cos()) / 2.0;
    1.0 - (1.0 - SMALLEST) * depth
}

/// A random seed for a turn's verbs.
pub fn seed() -> u64 {
    std::collections::hash_map::RandomState::new().hash_one(Instant::now())
}

/// The spinner verb `steps` into a turn whose verbs come from `seed`: a new one every
/// [`VERB_STEPS`]. The seed picks a start and a stride through the list that shares no factor
/// with its length, so every verb shows once before any repeats.
pub fn verb(seed: u64, steps: u64) -> &'static str {
    let len = VERBS.len() as u64;
    let slot = steps / VERB_STEPS;
    let start = seed % len;
    let mut stride = 1 + (seed / len) % (len - 1);
    while gcd(stride, len) != 1 {
        stride = stride % (len - 1) + 1;
    }
    VERBS[((start + slot % len * stride) % len) as usize]
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// How far each of a verb's `letters` is lifted `steps` into a turn, from 0 to 1. A crest
/// moves one letter a step, starting over with each new verb: the letter under it is lifted
/// all the way and its neighbours halfway. After the last letter it rests for [`WAVE_REST`]
/// steps, then runs again.
pub fn wave(steps: u64, letters: usize) -> impl Iterator<Item = f32> {
    let crest = (steps % VERB_STEPS) % (letters as u64 + WAVE_REST);
    (0..letters as u64).map(move |i| match i.abs_diff(crest) {
        0 => 1.0,
        1 => 0.5,
        _ => 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_breath_shrinks_for_a_second_and_grows_back() {
        let s = STEPS_PER_SECOND as u64;
        assert_eq!(breath(0), 1.0);
        assert!((breath(s) - SMALLEST).abs() < 1e-5, "smallest after a second");
        assert!((breath(2 * s) - 1.0).abs() < 1e-5, "full again after two");
        assert!((1..s).all(|n| breath(n) < breath(n - 1)), "shrinking");
        assert!((s + 1..2 * s).all(|n| breath(n) > breath(n - 1)), "growing");
        assert!((0..100).all(|n| (SMALLEST..=1.0).contains(&breath(n))));
    }

    #[test]
    fn steps_wake_on_a_grid() {
        let every = Duration::from_secs(1) / STEPS_PER_SECOND;
        assert_eq!(step(every * 3 + Duration::from_millis(5)), (3, every - Duration::from_millis(5)));
        assert_eq!(step(every * 3), (3, every));
        assert_eq!(step(Duration::ZERO), (0, every));
    }

    #[test]
    fn verbs_hold_then_change_without_repeating() {
        for seed in [0, 1, 42, 187, 188, u64::MAX, super::seed()] {
            assert_eq!(verb(seed, 0), verb(seed, VERB_STEPS - 1), "one verb for its whole slot");
            let shown: std::collections::HashSet<_> =
                (0..VERBS.len() as u64).map(|slot| verb(seed, slot * VERB_STEPS)).collect();
            assert_eq!(shown.len(), VERBS.len(), "seed {seed}: every verb once before any repeats");
        }
        assert!(VERBS.iter().all(|v| !v.is_empty() && !v.ends_with('…')));
    }

    #[test]
    fn the_wave_runs_one_letter_a_step_then_rests() {
        let lifts = |steps| wave(steps, 4).collect::<Vec<_>>();
        assert_eq!(lifts(0), [1.0, 0.5, 0.0, 0.0]);
        assert_eq!(lifts(1), [0.5, 1.0, 0.5, 0.0]);
        assert_eq!(lifts(3), [0.0, 0.0, 0.5, 1.0]);
        assert_eq!(lifts(4), [0.0, 0.0, 0.0, 0.5], "the last letter settles");
        assert_eq!(lifts(5), [0.0; 4], "then it rests");
        assert_eq!(lifts(4 + WAVE_REST), [1.0, 0.5, 0.0, 0.0], "and runs again");
        assert_eq!(lifts(VERB_STEPS), lifts(0), "a new verb starts its own wave");
    }
}
