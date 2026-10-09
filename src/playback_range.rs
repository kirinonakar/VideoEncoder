#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackRange {
    pub start: f32,
    pub end: f32,
}

impl PlaybackRange {
    pub fn new(duration: f32, start: f32, end: f32, limited: bool) -> Self {
        let duration = duration.max(0.0);
        if limited {
            let start = start.clamp(0.0, duration);
            Self {
                start,
                end: end.clamp(start, duration),
            }
        } else {
            Self {
                start: 0.0,
                end: duration,
            }
        }
    }

    pub fn clamp(self, time: f32) -> f32 {
        time.clamp(self.start, self.end)
    }

    pub fn play_from(self, time: f32) -> f32 {
        let time = self.clamp(time);
        if time >= self.end - 0.05 {
            self.start
        } else {
            time
        }
    }

    pub fn finished(self, time: f32) -> bool {
        time >= self.end
    }
}

#[cfg(test)]
mod tests {
    use super::PlaybackRange;

    #[test]
    fn limited_playback_clamps_seeks_and_restarts_at_selection_start() {
        let range = PlaybackRange::new(30.0, 5.0, 12.0, true);
        assert_eq!(
            [range.clamp(0.0), range.clamp(8.0), range.clamp(30.0)],
            [5.0, 8.0, 12.0]
        );
        assert_eq!(range.play_from(12.0), 5.0);
        assert_eq!(range.play_from(8.0), 8.0);
        assert!(!range.finished(11.99));
        assert!(range.finished(12.0));
        assert!(range.finished(12.01));
    }

    #[test]
    fn disabling_limit_restores_full_video_seek_and_rewind() {
        let range = PlaybackRange::new(30.0, 5.0, 12.0, false);
        assert_eq!(range.start, 0.0);
        assert_eq!(range.clamp(25.0), 25.0);
        assert!(!range.finished(12.0));
        assert_eq!(range.play_from(30.0), 0.0);
    }

    #[test]
    fn live_selection_changes_clamp_against_current_bounds() {
        let extended = PlaybackRange::new(30.0, 5.0, 20.0, true);
        let shortened = PlaybackRange::new(30.0, 16.0, 18.0, true);
        assert!(!extended.finished(15.0));
        assert_eq!(shortened.clamp(15.0), 16.0);
        assert_eq!(shortened.clamp(19.0), 18.0);
        let empty = PlaybackRange::new(0.0, 5.0, 12.0, true);
        assert_eq!(empty.clamp(5.0), 0.0);
        assert!(empty.finished(0.0));
    }
}
