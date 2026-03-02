//! Audio utilities — currently a thin module kept separate from `player`
//! so that DSP / equaliser code can live here in future without bloating
//! the player state machine.

pub mod resampler;
