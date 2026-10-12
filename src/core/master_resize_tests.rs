//! Production layout algorithms plus the final hinted resize planner, including
//! tiny-gap degeneracy and intentional overlapping layout families.
use super::layout::*;
use super::master_resize::*;
use super::types::Rect;

type Calculate = fn(&LayoutParams, &[LayoutClient<u32>]) -> Vec<LayoutResult<u32>>;
const LAYOUTS: &[(&str, Calculate)] = &[
    ("tile", calculate_tile),
    ("fibonacci", calculate_fibonacci),
    ("centeredmaster", calculate_centered_master),
    ("bstack", calculate_bstack),
    ("grid", calculate_grid),
    ("deck", calculate_deck),
    ("threecol", calculate_three_col),
    ("tatami", calculate_tatami),
    ("monocle", calculate_monocle),
    ("fullscreen", calculate_fullscreen),
    ("vstack", calculate_vstack),
];
fn overlap(a: ResizeFrame<u32>, b: ResizeFrame<u32>) -> bool {
    let (ax, ay, bx, by) = (
        i64::from(a.rect.x),
        i64::from(a.rect.y),
        i64::from(b.rect.x),
        i64::from(b.rect.y),
    );
    ax < bx + i64::from(b.rect.w) + 2 * i64::from(b.border)
        && bx < ax + i64::from(a.rect.w) + 2 * i64::from(a.border)
        && ay < by + i64::from(b.rect.h) + 2 * i64::from(b.border)
        && by < ay + i64::from(a.rect.h) + 2 * i64::from(a.border)
}
fn verify(frames: &[ResizeFrame<u32>], out: &[ResizeFrame<u32>]) {
    assert_eq!(frames.len(), out.len());
    for i in 0..frames.len() {
        assert_eq!(frames[i].client, out[i].client);
        assert!(out[i].rect.w > 0 && out[i].rect.h > 0);
        for j in i + 1..frames.len() {
            if !overlap(frames[i], frames[j]) {
                assert!(!overlap(out[i], out[j]), "new overlap {i},{j}: {out:?}");
            }
        }
    }
}
#[test]
fn all_native_layouts_resize_both_axes_without_new_overlaps_or_arrange_drift() {
    let fit = |_: u32, w, h| (w, h);
    let mut accepted = 0;
    for &(name, calculate) in LAYOUTS {
        for (width, height) in [(1600, 900), (900, 1600), (91, 73), (1, 1)] {
            for n in [1, 2, 4, 7, 13] {
                for masters in [0, 1, 2, n + 2] {
                    for requested_gap in [0, 10, 50] {
                        let gap = if matches!(name, "monocle" | "fullscreen") {
                            0
                        } else {
                            requested_gap
                        };
                        let border = if name == "fullscreen" { 0 } else { 2 };
                        let p = LayoutParams {
                            screen_area: Rect::new(-1600, 40, width, height),
                            n_master: masters,
                            m_fact: 0.55,
                            gap,
                        };
                        let clients: Vec<_> = (0..n)
                            .map(|key| LayoutClient {
                                key,
                                factor: 1.0,
                                border_w: border,
                            })
                            .collect();
                        let frames: Vec<_> = calculate(&p, &clients)
                            .into_iter()
                            .map(|r| ResizeFrame {
                                client: r.key,
                                rect: r.rect,
                                border,
                            })
                            .collect();
                        for anchor in &frames {
                            for horizontal in [true, false] {
                                for delta in [-0.12, 0.12] {
                                    let start = intent_from_rect(
                                        anchor.client,
                                        anchor.rect,
                                        p.screen_area,
                                        gap,
                                        border,
                                    );
                                    let wanted = adjust_intent(start, horizontal, delta).unwrap();
                                    if let Some((effective, out)) = bounded_plan(
                                        p.screen_area,
                                        gap,
                                        &frames,
                                        start,
                                        wanted,
                                        &fit,
                                    ) {
                                        verify(&frames, &out);
                                        accepted += 1;
                                        assert_eq!(
                                            out,
                                            exact_plan(
                                                p.screen_area,
                                                gap,
                                                &frames,
                                                effective,
                                                &fit
                                            )
                                            .unwrap()
                                        );
                                        if width >= 900 && frames.len() == 1 && delta < 0.0 {
                                            let resized = out
                                                .iter()
                                                .find(|f| f.client == anchor.client)
                                                .unwrap();
                                            assert!(
                                                if horizontal {
                                                    resized.rect.w < anchor.rect.w
                                                } else {
                                                    resized.rect.h < anchor.rect.h
                                                },
                                                "{name} singleton did not shrink"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(accepted > 10000);
}
#[test]
fn scrolling_columns_preserve_separation_and_resize_width_and_single_row_height() {
    let p = ScrollingParams {
        screen_area: Rect::new(-1600, 40, 1600, 900),
        column_width_ratio: 0.55,
        column_width_factors: vec![1.0; 3],
        gap: 10,
        viewport_x: 0.0,
    };
    let columns = vec![
        vec![LayoutClient {
            key: 0,
            factor: 1.0,
            border_w: 2,
        }],
        vec![
            LayoutClient {
                key: 1,
                factor: 1.0,
                border_w: 2,
            },
            LayoutClient {
                key: 2,
                factor: 1.0,
                border_w: 2,
            },
        ],
        vec![LayoutClient {
            key: 3,
            factor: 1.0,
            border_w: 2,
        }],
    ];
    let (results, _) = calculate_scrolling(&p, &columns, 0);
    let frames: Vec<_> = results
        .into_iter()
        .map(|r| ResizeFrame {
            client: r.key,
            rect: r.rect,
            border: 2,
        })
        .collect();
    for horizontal in [true, false] {
        let start = intent_from_rect(0, frames[0].rect, p.screen_area, p.gap, 2);
        let wanted = adjust_intent(start, horizontal, -0.1).unwrap();
        let (_, out) = bounded_plan(p.screen_area, p.gap, &frames, start, wanted, &|_, w, h| {
            (w, h)
        })
        .unwrap();
        verify(&frames, &out);
        assert!(if horizontal {
            out[0].rect.w < frames[0].rect.w
        } else {
            out[0].rect.h < frames[0].rect.h
        });
    }
}
#[test]
fn tiny_grid_and_vstack_cannot_gain_the_reviewed_overlap() {
    for (calculate, n, gap, anchor) in [
        (calculate_grid as Calculate, 4, 50, 1),
        (calculate_vstack as Calculate, 7, 10, 0),
    ] {
        let p = LayoutParams {
            screen_area: Rect::new(-1600, 40, 91, 73),
            n_master: 1,
            m_fact: 0.55,
            gap,
        };
        let clients: Vec<_> = (0..n)
            .map(|key| LayoutClient {
                key,
                factor: 1.0,
                border_w: 2,
            })
            .collect();
        let frames: Vec<_> = calculate(&p, &clients)
            .into_iter()
            .map(|r| ResizeFrame {
                client: r.key,
                rect: r.rect,
                border: 2,
            })
            .collect();
        let start = intent_from_rect(anchor, frames[anchor as usize].rect, p.screen_area, gap, 2);
        let wanted = adjust_intent(start, true, 0.12).unwrap();
        if let Some((_, out)) =
            bounded_plan(p.screen_area, gap, &frames, start, wanted, &|_, w, h| {
                (w, h)
            })
        {
            verify(&frames, &out);
        }
    }
}
#[test]
fn hundred_rearranges_and_output_roundtrips_do_not_accumulate_geometry() {
    let clients: Vec<_> = (0..4)
        .map(|key| LayoutClient {
            key,
            factor: 1.0,
            border_w: 2,
        })
        .collect();
    for &(name, calculate) in LAYOUTS {
        let gap = if matches!(name, "monocle" | "fullscreen") {
            0
        } else {
            10
        };
        let mut p = LayoutParams {
            screen_area: Rect::new(-1600, 40, 1600, 900),
            n_master: 2,
            m_fact: 0.55,
            gap,
        };
        let calc = |p: &LayoutParams| {
            calculate(p, &clients)
                .into_iter()
                .map(|r| ResizeFrame {
                    client: r.key,
                    rect: r.rect,
                    border: 2,
                })
                .collect::<Vec<_>>()
        };
        let frames = calc(&p);
        let start = intent_from_rect(1, frames[1].rect, p.screen_area, gap, 2);
        let wanted = adjust_intent(start, true, -0.08).unwrap();
        if let Some((intent, expected)) =
            bounded_plan(p.screen_area, gap, &frames, start, wanted, &|_, w, h| {
                (w, h)
            })
        {
            for _ in 0..100 {
                assert_eq!(
                    expected,
                    exact_plan(p.screen_area, gap, &calc(&p), intent, &|_, w, h| (w, h)).unwrap()
                );
            }
            p.screen_area = Rect::new(100, 200, 1200, 700);
            let changed = calc(&p);
            if let Some(out) = exact_plan(p.screen_area, gap, &changed, intent, &|_, w, h| (w, h)) {
                verify(&changed, &out);
            }
            p.screen_area = Rect::new(-1600, 40, 1600, 900);
            assert_eq!(
                expected,
                exact_plan(p.screen_area, gap, &calc(&p), intent, &|_, w, h| (w, h)).unwrap()
            );
        }
    }
}
