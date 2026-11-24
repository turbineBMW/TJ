//! Chat transcript spike: a virtualized message list that follows the tail
//! while a reply streams in as markdown.

use std::time::{Duration, Instant};

use gpui_kit::component::{
    ActiveTheme as _,
    message_scroller::{MessageScroller, MessageScrollerState},
    text::{TextView, TextViewState},
};
use gpui_kit::*;
use gpui_kit::base::{h_flex, v_flex};

use crate::sample;

#[derive(Clone, Copy, PartialEq)]
enum Role {
    User,
    Agent,
}

#[derive(Clone)]
struct Message {
    role: Role,
    body: Entity<TextViewState>,
}

/// How fast the fake agent emits text.
#[derive(Clone, Copy, PartialEq)]
pub enum Speed {
    /// About what claude.ai shows: ~6 words every 100 ms.
    Realistic,
    /// A stress test: ~7,500 characters a second.
    Firehose,
}

impl Speed {
    fn chunking(self) -> (usize, Duration) {
        match self {
            Speed::Realistic => (6, Duration::from_millis(100)),
            Speed::Firehose => (20, Duration::from_millis(16)),
        }
    }
}

#[derive(Default, Clone)]
pub struct StreamStats {
    pub chars: usize,
    pub chunks: usize,
    pub elapsed: Duration,
    pub streaming: bool,
}

pub struct ChatView {
    messages: Vec<Message>,
    scroller: Entity<MessageScrollerState>,
    stream: Option<Task<()>>,
    pub stats: StreamStats,
}

impl ChatView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        Self { messages: Vec::new(), scroller, stream: None, stats: StreamStats::default() }
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    fn push(&mut self, role: Role, text: &str, cx: &mut Context<Self>) -> Entity<TextViewState> {
        let body = cx.new(|cx| TextViewState::markdown(text, cx).selectable(true));
        self.messages.push(Message { role, body: body.clone() });
        self.scroller.update(cx, |s, cx| {
            s.append(1, cx);
        });
        body
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.stream = None;
        self.messages.clear();
        self.stats = StreamStats::default();
        self.scroller.update(cx, |s, cx| s.reset(0, cx));
        cx.notify();
    }

    /// Add `pairs` finished prompt/reply pairs at once, to test a long history.
    pub fn add_history(&mut self, pairs: usize, cx: &mut Context<Self>) {
        for _ in 0..pairs {
            self.push(Role::User, sample::USER_PROMPT, cx);
            self.push(Role::Agent, sample::REPLY, cx);
        }
        cx.notify();
    }

    /// Send the sample prompt and stream the sample reply into a new message.
    pub fn stream_reply(&mut self, speed: Speed, cx: &mut Context<Self>) {
        self.push(Role::User, sample::USER_PROMPT, cx);
        let body = self.push(Role::Agent, "", cx);
        let row = self.messages.len() - 1;
        let chunks = chunk_words(sample::REPLY, speed.chunking().0);
        let interval = speed.chunking().1;

        self.stats = StreamStats { streaming: true, ..Default::default() };
        let started = Instant::now();

        self.stream = Some(cx.spawn(async move |this, cx| {
            for chunk in chunks {
                cx.background_executor().timer(interval).await;
                let alive = this.update(cx, |this, cx| {
                    body.update(cx, |state, cx| state.push_str(&chunk, cx));
                    // The list caches row heights, so the growing row has to be remeasured.
                    this.scroller.update(cx, |s, cx| {
                        s.remeasure_items(row..row + 1, cx);
                    });
                    this.stats.chars += chunk.len();
                    this.stats.chunks += 1;
                    this.stats.elapsed = started.elapsed();
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.stats.streaming = false;
                this.stats.elapsed = started.elapsed();
                cx.notify();
            });
        }));
        cx.notify();
    }
}

/// Split text into chunks of about `words` words, keeping all whitespace so
/// the concatenation is byte-identical to the input.
fn chunk_words(text: &str, words: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    let mut in_word = false;
    for ch in text.chars() {
        let is_space = ch.is_whitespace();
        if in_word && is_space {
            count += 1;
            if count >= words {
                chunks.push(std::mem::take(&mut current));
                count = 0;
            }
        }
        in_word = !is_space;
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let messages = self.messages.clone();
        let theme = cx.theme();
        let user_bg = theme.secondary;
        let bg = theme.background;

        let scroller = MessageScroller::new("transcript", self.scroller.clone(), move |ix, _window, cx| {
            let Some(message) = messages.get(ix) else {
                return div().into_any_element();
            };
            let body = TextView::new(&message.body).selectable(true);
            match message.role {
                Role::User => h_flex()
                    .w_full()
                    .px_4()
                    .py_2()
                    .justify_end()
                    .child(
                        div()
                            .max_w(relative(0.75))
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .bg(user_bg)
                            .child(body),
                    )
                    .into_any_element(),
                Role::Agent => {
                    let _ = cx;
                    div().w_full().px_4().py_2().child(body.stream_fade(true)).into_any_element()
                }
            }
        })
        .scrollbar(true)
        .jump_button(true)
        .with_bottom_fade(bg);

        v_flex().size_full().bg(bg).child(scroller)
    }
}

#[cfg(test)]
mod tests {
    use super::chunk_words;

    #[test]
    fn chunks_rejoin_to_the_input() {
        let text = crate::sample::REPLY;
        assert_eq!(chunk_words(text, 6).concat(), text);
        assert_eq!(chunk_words(text, 1).concat(), text);
    }
}
