//! Cursor sprite selection shared by the desktop companions.

/// A move at least this long is fly.
pub const FLY_DISTANCE_PX: u32 = 256;
/// A move at least this fast is fly.
pub const FLY_SPEED_PX_PER_SEC: u32 = 1_200;
pub const MOVE_FRESH_MS: u64 = 200;
pub const CLICK_FRESH_MS: u64 = 200;
pub const TYPE_FRESH_MS: u64 = 400;
pub const CURSOR_FRAME_MS: u64 = 100;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorPose {
    System,
    Pointer,
    Crawl,
    Fly,
    Reach,
    Click,
    Type,
    Busy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MotionKind {
    None,
    Move,
    Click,
    Drag,
    Type,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Motion {
    pub kind: MotionKind,
    pub age_ms: u64,
    pub distance_px: u32,
    pub elapsed_ms: u32,
    pub clickable: bool,
}

pub fn pose(visible: bool, running: bool, motion: Option<Motion>) -> CursorPose {
    if !visible {
        return CursorPose::System;
    }
    let Some(motion) = motion else {
        return if running {
            CursorPose::Busy
        } else {
            CursorPose::Pointer
        };
    };
    if motion.kind == MotionKind::Click && motion.age_ms < CLICK_FRESH_MS {
        return CursorPose::Click;
    }
    if motion.kind == MotionKind::Type && motion.age_ms < TYPE_FRESH_MS {
        return CursorPose::Type;
    }
    if matches!(motion.kind, MotionKind::Move | MotionKind::Drag) && motion.age_ms < MOVE_FRESH_MS {
        return if flies(&motion) {
            CursorPose::Fly
        } else {
            CursorPose::Crawl
        };
    }
    if motion.clickable && matches!(motion.kind, MotionKind::Move | MotionKind::Drag) {
        return CursorPose::Reach;
    }
    if running {
        CursorPose::Busy
    } else {
        CursorPose::Pointer
    }
}

fn flies(motion: &Motion) -> bool {
    if motion.distance_px >= FLY_DISTANCE_PX {
        return true;
    }
    if motion.elapsed_ms == 0 {
        return false;
    }
    let speed = u64::from(motion.distance_px) * 1000 / u64::from(motion.elapsed_ms);
    speed >= u64::from(FLY_SPEED_PX_PER_SEC)
}

pub fn cursor_frame(pose: CursorPose, age_ms: u64) -> usize {
    let step = CURSOR_FRAME_MS.max(1);
    match pose {
        CursorPose::Click => ((age_ms / step) as usize).min(1),
        CursorPose::Type => (age_ms / step) as usize % 2,
        CursorPose::Busy => (age_ms / step) as usize % 4,
        _ => 0,
    }
}

pub fn cursor_frame_name(pose: CursorPose, frame: usize) -> &'static str {
    match pose {
        CursorPose::System | CursorPose::Pointer => "pointer",
        CursorPose::Crawl => "crawl",
        CursorPose::Fly => "fly",
        CursorPose::Reach => "reach",
        CursorPose::Click => ["click1", "click2"][frame.min(1)],
        CursorPose::Type => ["type1", "type2"][frame.min(1)],
        CursorPose::Busy => ["busy1", "busy2", "busy3", "busy4"][frame % 4],
    }
}
