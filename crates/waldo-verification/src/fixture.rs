use std::fmt;

use serde::{Deserialize, Serialize};

/// A reproducible pseudo-random sequence for fixture layout and payload choices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeededSequence {
    state: u64,
}

impl SeededSequence {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    pub fn index(&mut self, upper_bound: u32) -> Option<u32> {
        (upper_bound > 0).then(|| (self.next_u64() % u64::from(upper_bound)) as u32)
    }
}

/// An integer rectangle in fixture-image coordinates.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One independently recorded fixture target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixturePoint {
    pub id: String,
    pub x: u32,
    pub y: u32,
}

/// Seeded targets distributed across every cell of a fixture rectangle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetGrid {
    pub seed: u64,
    pub bounds: FixtureRect,
    pub columns: u32,
    pub rows: u32,
    pub targets: Vec<FixturePoint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetGridError {
    EmptyBounds,
    EmptyGrid,
    GridExceedsBounds,
    CoordinateOverflow,
}

impl fmt::Display for TargetGridError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyBounds => "fixture bounds must have non-zero dimensions",
            Self::EmptyGrid => "target grid must have at least one row and column",
            Self::GridExceedsBounds => "target grid cells cannot be smaller than one pixel",
            Self::CoordinateOverflow => "fixture bounds exceed the coordinate range",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for TargetGridError {}

impl TargetGrid {
    pub fn generate(
        bounds: FixtureRect,
        columns: u32,
        rows: u32,
        seed: u64,
    ) -> Result<Self, TargetGridError> {
        validate_bounds(bounds)?;
        if columns == 0 || rows == 0 {
            return Err(TargetGridError::EmptyGrid);
        }
        if columns > bounds.width || rows > bounds.height {
            return Err(TargetGridError::GridExceedsBounds);
        }

        let target_count = columns
            .checked_mul(rows)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or(TargetGridError::CoordinateOverflow)?;
        let mut sequence = SeededSequence::new(seed);
        let mut targets = Vec::with_capacity(target_count);
        for row in 0..rows {
            let start_y = partition_start(bounds.y, bounds.height, row, rows)?;
            let end_y = partition_start(bounds.y, bounds.height, row + 1, rows)?;
            for column in 0..columns {
                let start_x = partition_start(bounds.x, bounds.width, column, columns)?;
                let end_x = partition_start(bounds.x, bounds.width, column + 1, columns)?;
                let x = start_x + sequence.index(end_x - start_x).expect("non-empty cell");
                let y = start_y + sequence.index(end_y - start_y).expect("non-empty cell");
                targets.push(FixturePoint {
                    id: format!("cell-{row}-{column}"),
                    x,
                    y,
                });
            }
        }

        Ok(Self {
            seed,
            bounds,
            columns,
            rows,
            targets,
        })
    }
}

/// Deterministic in-bounds points at the corners, edges, and centre.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryTargets {
    pub bounds: FixtureRect,
    pub targets: Vec<FixturePoint>,
}

impl BoundaryTargets {
    pub fn generate(bounds: FixtureRect) -> Result<Self, TargetGridError> {
        validate_bounds(bounds)?;
        let right = bounds.x + bounds.width - 1;
        let bottom = bounds.y + bounds.height - 1;
        let centre_x = bounds.x + (bounds.width - 1) / 2;
        let centre_y = bounds.y + (bounds.height - 1) / 2;
        let targets = [
            ("top-left", bounds.x, bounds.y),
            ("top", centre_x, bounds.y),
            ("top-right", right, bounds.y),
            ("right", right, centre_y),
            ("bottom-right", right, bottom),
            ("bottom", centre_x, bottom),
            ("bottom-left", bounds.x, bottom),
            ("left", bounds.x, centre_y),
            ("centre", centre_x, centre_y),
        ]
        .into_iter()
        .map(|(id, x, y)| FixturePoint {
            id: id.to_owned(),
            x,
            y,
        })
        .collect();
        Ok(Self { bounds, targets })
    }
}

fn validate_bounds(bounds: FixtureRect) -> Result<(), TargetGridError> {
    if bounds.width == 0 || bounds.height == 0 {
        return Err(TargetGridError::EmptyBounds);
    }
    bounds
        .x
        .checked_add(bounds.width)
        .and_then(|_| bounds.y.checked_add(bounds.height))
        .ok_or(TargetGridError::CoordinateOverflow)?;
    Ok(())
}

fn partition_start(
    origin: u32,
    length: u32,
    index: u32,
    partitions: u32,
) -> Result<u32, TargetGridError> {
    let offset = u64::from(length) * u64::from(index) / u64::from(partitions);
    u64::from(origin)
        .checked_add(offset)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(TargetGridError::CoordinateOverflow)
}
