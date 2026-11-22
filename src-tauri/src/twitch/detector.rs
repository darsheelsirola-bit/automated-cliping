use std::collections::{HashMap, HashSet, VecDeque};

const BASELINE_SAMPLES: usize = 120;
const WARMUP_SAMPLES: usize = 12;
const COOLDOWN_MS: i64 = 45_000;

struct Message {
    timestamp_ms: i64,
    chatter: String,
    normalized: String,
    emotes: usize,
}

pub struct Sample {
    pub per_second: usize,
    pub per_five: usize,
    pub per_thirty: usize,
    pub unique: usize,
    pub repeated: usize,
    pub emotes: usize,
    pub baseline: f64,
    pub score: Option<f64>,
}

#[derive(Default)]
pub struct Detector {
    messages: VecDeque<Message>,
    history: VecDeque<f64>,
    exclude_until_ms: i64,
    last_trigger_ms: Option<i64>,
    consecutive: u8,
}

fn median(mut numbers: Vec<f64>) -> f64 {
    numbers.sort_by(f64::total_cmp);
    let middle = numbers.len() / 2;
    match numbers.len() {
        0 => 0.0,
        len if len % 2 == 0 => (numbers[middle - 1] + numbers[middle]) / 2.0,
        _ => numbers[middle],
    }
}

impl Detector {
    pub fn message(&mut self, timestamp_ms: i64, chatter: String, text: &str, emotes: usize) {
        let normalized = text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        self.messages.push_back(Message {
            timestamp_ms,
            chatter,
            normalized,
            emotes,
        });
        if self.messages.len() > 60_000 {
            self.messages.pop_front();
        }
    }

    pub fn suppress_until(&mut self, timestamp_ms: i64) {
        self.exclude_until_ms = timestamp_ms;
        self.consecutive = 0;
        self.messages.clear();
    }

    // Call once per five-second interval. Audio evidence is normalized to 0–1.
    pub fn sample(&mut self, now_ms: i64, audio_reaction: Option<f64>, threshold: f64) -> Sample {
        self.messages
            .retain(|message| (now_ms - 30_000..=now_ms).contains(&message.timestamp_ms));
        let mut senders = HashSet::new();
        let mut phrases = HashMap::new();
        let mut per_second = 0;
        let mut per_five = 0usize;
        let mut emotes = 0;
        for message in &self.messages {
            if message.timestamp_ms <= now_ms - 5_000 {
                continue;
            }
            per_five += 1;
            per_second += usize::from(message.timestamp_ms > now_ms - 1_000);
            emotes += message.emotes;
            senders.insert(&message.chatter);
            *phrases.entry(&message.normalized).or_insert(0usize) += 1;
        }
        let unique = senders.len();
        let repeated: usize = phrases.values().map(|count| count.saturating_sub(1)).sum();
        let effective = (unique + per_five.saturating_sub(repeated).min(unique * 2)) as f64 / 2.0;
        let baseline = median(self.history.iter().copied().collect());
        let mad = median(
            self.history
                .iter()
                .map(|value| (value - baseline).abs())
                .collect(),
        );
        let deviation = (effective - baseline) / (mad * 1.4826).max(baseline.sqrt()).max(1.0);
        let enough_history = self.history.len() >= WARMUP_SAMPLES;
        let candidate = enough_history
            && now_ms >= self.exclude_until_ms
            && unique >= 3
            && deviation >= 3.0
            && effective >= baseline * 1.4;
        let mut score = None;
        if candidate {
            self.consecutive = self.consecutive.saturating_add(1);
            let audio = audio_reaction.unwrap_or_default().clamp(0.0, 1.0);
            let rank = (deviation * 6.0).min(45.0)
                + (unique as f64 * 2.5).min(20.0)
                + audio * 20.0
                + (f64::from(self.consecutive) * 7.5).min(15.0);
            if self.consecutive >= 2
                && rank >= threshold
                && self
                    .last_trigger_ms
                    .is_none_or(|last| now_ms - last >= COOLDOWN_MS)
            {
                self.last_trigger_ms = Some(now_ms);
                score = Some(rank);
            }
        } else {
            self.consecutive = 0;
            if now_ms >= self.exclude_until_ms {
                self.history.push_back(effective);
                if self.history.len() > BASELINE_SAMPLES {
                    self.history.pop_front();
                }
            }
        }
        let voice = audio_reaction.unwrap_or_default();
        if score.is_none()
            && enough_history
            && now_ms >= self.exclude_until_ms
            && voice >= 0.78
            && voice * 100.0 >= threshold
            && self
                .last_trigger_ms
                .is_none_or(|last| now_ms - last >= COOLDOWN_MS)
        {
            self.last_trigger_ms = Some(now_ms);
            score = Some(voice * 100.0);
        }
        Sample {
            per_second,
            per_five,
            per_thirty: self.messages.len(),
            unique,
            repeated,
            emotes,
            baseline,
            score,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_voice_can_trigger_once_during_quiet_chat() {
        let mut detector = Detector::default();
        for tick in 1..=12 {
            assert!(detector
                .sample(tick * 5000, Some(0.99), 60.0)
                .score
                .is_none());
        }
        assert!(detector.sample(65000, Some(0.7), 60.0).score.is_none());
        assert!(detector.sample(70000, Some(0.88), 60.0).score.is_some());
        assert!(detector.sample(75000, Some(0.98), 60.0).score.is_none());
        detector.suppress_until(180000);
        assert!(detector.sample(150000, Some(1.0), 60.0).score.is_none());
    }

    #[test]
    fn rejects_spam_and_detects_persistent_diverse_burst_once() {
        let mut detector = Detector::default();
        for tick in 1..=12 {
            detector.sample(tick * 5000, None, 60.0);
        }
        for tick in 13..=14 {
            for _ in 0..200 {
                detector.message(tick * 5000, "bot".into(), "same line", 0);
            }
            assert!(detector.sample(tick * 5000, None, 60.0).score.is_none());
        }
        for tick in 15..=18 {
            for user in 0..15 {
                detector.message(
                    tick * 5000,
                    user.to_string(),
                    &format!("reaction {user}"),
                    0,
                );
            }
            let sample = detector.sample(tick * 5000, None, 60.0);
            assert_eq!(sample.score.is_some(), tick == 16);
            if tick == 16 {
                assert!(sample.score.unwrap() >= 80.0);
            }
        }
    }

    #[test]
    fn raid_exclusion_prevents_detection_and_baseline_pollution() {
        let mut detector = Detector::default();
        detector.suppress_until(40_000);
        for tick in 1..=7 {
            for user in 0..50 {
                detector.message(tick * 5000, user.to_string(), "raid", 0);
            }
            assert!(detector.sample(tick * 5000, None, 60.0).score.is_none());
        }
        assert!(detector.history.is_empty());
    }
}
