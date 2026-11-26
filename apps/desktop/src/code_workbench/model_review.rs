use super::*;
use gpui_component::checkbox::Checkbox;
use locale::text;
use vibex_core::{ModelReviewTarget, VibexSessionId};

mod state;
use state::{DiffSelection, FileSides, Notice, Phase, ReviewError};

#[derive(Clone)]
struct Origin {
    session: VibexSessionId,
    workspace: WorkspaceId,
    generation: u64,
    backend: BackendFacade,
}

impl Origin {
    fn capture(workbench: &CodeWorkbench, cx: &App) -> Option<Self> {
        if !cfg!(windows) {
            return None;
        }
        let workspace = workbench.workspace.as_ref()?;
        let parent = workbench.parent.as_ref()?.upgrade()?;
        let parent = parent.read(cx);
        let session = parent.model_review_session(&workspace.id)?;
        let backend = parent.backend_facade()?;
        if workbench.pending_workspace.is_some()
            || !Arc::ptr_eq(backend.git(), workbench.backend.as_ref()?.git())
        {
            return None;
        }
        Some(Self {
            session,
            workspace: workspace.id.clone(),
            generation: workspace.generation,
            backend,
        })
    }

    fn matches(&self, other: &Self) -> bool {
        self.session == other.session
            && self.workspace == other.workspace
            && self.generation == other.generation
            && Arc::ptr_eq(self.backend.git(), other.backend.git())
    }

    async fn cancel(self, ticket: RequestId) -> vibex_backend::BackendResult<()> {
        self.backend
            .git()
            .cancel_model_review(self.session, self.workspace, ticket)
            .await
    }
}

pub(super) struct ModelReview {
    id: RequestId,
    origin: Origin,
    selection: DiffSelection,
    choices: Vec<ModelReviewTarget>,
    target: Option<usize>,
    phase: Phase<RequestId, SharedString>,
    notice: Option<String>,
}

fn review_job<T: Send + 'static>(
    cx: &mut App,
    work: impl Future<Output = vibex_backend::BackendResult<T>> + Send + 'static,
) -> Task<Result<T, ReviewError>> {
    let runner = gpui_tokio::Tokio::spawn(cx, work);
    cx.spawn(async move |_| {
        runner
            .await
            .map_err(|_| {
                ReviewError::Other(text(
                    "Review task stopped. Reopen Model review and try again.",
                    "审查任务已停止，请重新打开模型审查后重试。",
                    "審查工作已停止，請重新開啟模型審查後重試。",
                )
                .to_owned())
            })?
            .map_err(|error| {
                let mut message = locale::localize_error_message(&error.message);
                let recovery = match error.code.as_str() {
                    "git_review_invalid_snapshot" | "git_review_unavailable" | "git_review_selection" => Some(text("Refresh Git and select fewer, complete tracked text changes.", "请刷新 Git 状态，减少勾选项，并只选择已跟踪文件的完整文本更改。", "請重新整理 Git 狀態，減少勾選項目，並只選擇已追蹤檔案的完整文字變更。")),
                    "model_review_target_changed" => Some(text("Reopen Model review to refresh the configured models.", "请重新打开模型审查以刷新模型列表。", "請重新開啟模型審查以重新整理模型清單。")),
                    "model_review_unavailable" => Some(text("Connect to the local desktop backend to review these changes.", "请连接到本机桌面后端以审查这些更改。", "請連線至本機桌面後端以審查這些變更。")),
                    "model_review_credential_unavailable" => Some(text("Check the selected provider’s API key in settings.", "请在设置中检查所选供应商的 API 密钥。", "請在設定中檢查所選供應商的 API 金鑰。")),
                    "model_review_http_failed" | "model_review_timeout" | "model_review_transport_failed" => Some(text("Check the provider before starting another review. The previous request may have reached it.", "请先检查供应商状态，再决定是否重新审查。上一个请求可能已送达。", "請先檢查供應商狀態，再決定是否重新審查。上一個請求可能已送達。")),
                    _ => None,
                };
                if let Some(hint) = error.recovery_hint.as_deref().or(recovery) {
                    message.push('\n');
                    message.push_str(&locale::localize_error_message(hint));
                }
                if error.code == "model_review_ticket_invalid" { ReviewError::TicketInvalid(message) } else { ReviewError::Other(message) }
            })
    })
}

fn target_name(target: &ModelReviewTarget) -> String {
    format!("{} · {}", target.provider_name, target.model_id)
}

impl Notice {
    fn localized(self) -> String {
        match self {
            Self::Recorded => text(
                "Review recorded in this session.",
                "审查结果已记入本会话。",
                "審查結果已記入本會話。",
            ),
            Self::Cancelled { sent: true } => text(
                "Cancelled. The provider may still charge for this request.",
                "已取消。供应商仍可能对已发送的请求计费。",
                "已取消。供應商仍可能對已送出的請求計費。",
            ),
            Self::Cancelled { sent: false } => text(
                "Preview cancelled. Nothing was sent.",
                "已取消预览，未发送请求。",
                "已取消預覽，未送出請求。",
            ),
            Self::Error(message) => return message,
        }
        .to_owned()
    }
}

impl CodeRightRail {
    pub(super) fn eligible_for_model_review(&self, cx: &App) -> bool {
        if !cfg!(windows) {
            return false;
        }
        let workbench = self.workbench.read(cx);
        workbench.workspace.as_ref().is_some_and(|workspace| {
            workbench
                .parent
                .as_ref()
                .and_then(WeakEntity::upgrade)
                .is_some_and(|parent| {
                    parent
                        .read(cx)
                        .model_review_session(&workspace.id)
                        .is_some()
                })
        })
    }

    pub(super) fn sync_model_review_context(&mut self, cx: &mut Context<Self>) {
        let eligible = self.eligible_for_model_review(cx);
        let workbench = self.workbench.read(cx);
        let stale = self.model_review.as_ref().is_some_and(|review| {
            !Origin::capture(workbench, cx).is_some_and(|origin| review.origin.matches(&origin))
        });
        if stale
            && let Some(review) = self.model_review.take()
            && let Some(ticket) = review.phase.unsent_ticket().cloned()
        {
            review_job(cx, review.origin.cancel(ticket)).detach();
        }
        if self.model_review_eligible != Some(eligible) || stale {
            self.model_review_eligible = Some(eligible);
            cx.notify();
        }
    }

    fn current_review(
        &mut self,
        id: &RequestId,
        cx: &mut Context<Self>,
    ) -> Option<&mut ModelReview> {
        self.sync_model_review_context(cx);
        self.model_review.as_mut().filter(|review| &review.id == id)
    }

    pub(super) fn open_model_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model_review.is_some() {
            return;
        }
        let workbench = self.workbench.read(cx);
        let Some(origin) = Origin::capture(workbench, cx) else {
            hint_layer::push(
                window,
                hint_notification(
                    NotificationType::Error,
                    text(
                        "Reconnect the backend and select a timeline session in this workspace.",
                        "请重新连接后端，并选择本工作区的时间线会话。",
                        "請重新連線後端，並選擇本工作區的時間線會話。",
                    ),
                ),
                cx,
            );
            return;
        };
        let Some(status) = workbench
            .git_status()
            .filter(|status| status.workspace_id == origin.workspace)
        else {
            hint_layer::push(
                window,
                hint_notification(
                    NotificationType::Error,
                    text(
                        "Refresh Git status before opening Model review.",
                        "请先刷新 Git 状态，再打开模型审查。",
                        "請先重新整理 Git 狀態，再開啟模型審查。",
                    ),
                ),
                cx,
            );
            return;
        };
        let selection = DiffSelection::new(status.changes.iter().map(|change| {
            if change.kind == GitChangeKind::Untracked {
                FileSides::Untracked
            } else {
                FileSides::Tracked {
                    path: &change.path,
                    staged: change.staged,
                    unstaged: change.unstaged,
                }
            }
        }));
        let id = RequestId::new();
        let backend = origin.backend.clone();
        self.model_review = Some(ModelReview {
            id: id.clone(),
            origin,
            selection,
            choices: Vec::new(),
            target: None,
            phase: Phase::Loading,
            notice: None,
        });
        let job = review_job(
            cx,
            async move { backend.git().model_review_choices().await },
        );
        cx.spawn(async move |rail, cx| {
            let result = job.await;
            let _ = rail.update(cx, |rail, cx| {
                let Some(review) = rail.current_review(&id, cx) else { return };
                match result {
                    Ok(choices) if !choices.is_empty() => { review.choices = choices; review.phase = Phase::Choosing; }
                    result => {
                        review.phase = Phase::Finished;
                        review.notice = Some(result.err().map(ReviewError::message).unwrap_or_else(|| text("No eligible model. Configure an enabled OpenAI Responses or Chat Completions model, then reopen Model review.", "没有可用模型。请配置并启用支持 OpenAI Responses 或 Chat Completions 的模型，再重新打开审查。", "沒有可用模型。請設定並啟用支援 OpenAI Responses 或 Chat Completions 的模型，再重新開啟審查。").to_owned()));
                    }
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn preview_model_review(&mut self, id: RequestId, cx: &mut Context<Self>) {
        let Some(review) = self.current_review(&id, cx) else {
            return;
        };
        if !matches!(review.phase, Phase::Choosing) {
            return;
        }
        let origin = review.origin.clone();
        let selection: Vec<_> = review
            .selection
            .selected()
            .map(|(path, staged)| GitDiffRequest {
                workspace_id: origin.workspace.clone(),
                path: path.clone(),
                staged: *staged,
            })
            .collect();
        if selection.is_empty() {
            review.notice = Some(
                text(
                    "Select at least one staged or unstaged change.",
                    "请至少勾选一项已暂存或未暂存的更改。",
                    "請至少勾選一項已暫存或未暫存的變更。",
                )
                .to_owned(),
            );
            cx.notify();
            return;
        }
        let Some(target) = review
            .target
            .and_then(|index| review.choices.get(index))
            .cloned()
        else {
            review.notice = Some(
                text(
                    "Choose a provider and model.",
                    "请选择供应商和模型。",
                    "請選擇供應商和模型。",
                )
                .to_owned(),
            );
            cx.notify();
            return;
        };
        review.notice = None;
        review.phase = Phase::Previewing;
        let cleanup = origin.clone();
        let job = review_job(cx, async move {
            origin
                .backend
                .git()
                .preview_model_review(origin.session, origin.workspace, selection, target)
                .await
        });
        cx.spawn(async move |rail, cx| {
            let result = job.await;
            let mut unclaimed = result.as_ref().ok().map(|preview| preview.ticket.clone());
            let _ = rail.update(cx, |rail, cx| {
                let Some(review) = rail.current_review(&id, cx) else {
                    return;
                };
                match result {
                    Ok(preview) => {
                        review.phase = Phase::Ready(preview.ticket, preview.text.into());
                        unclaimed = None;
                    }
                    Err(error) => {
                        review.phase = Phase::Choosing;
                        review.notice = Some(error.message());
                    }
                }
                cx.notify();
            });
            if let Some(ticket) = unclaimed {
                let _ = gpui_tokio::Tokio::spawn(cx, cleanup.cancel(ticket)).await;
            }
        })
        .detach();
        cx.notify();
    }

    fn send_model_review(&mut self, id: RequestId, cx: &mut Context<Self>) {
        let Some(review) = self.current_review(&id, cx) else {
            return;
        };
        let Some(ticket) = review.phase.begin_send() else {
            return;
        };
        let origin = review.origin.clone();
        let job = review_job(cx, async move {
            origin
                .backend
                .git()
                .send_model_review(origin.session, origin.workspace, ticket)
                .await
        });
        cx.spawn(async move |rail, cx| {
            let result = job.await;
            let _ = rail.update(cx, |rail, cx| {
                let Some(review) = rail.current_review(&id, cx) else {
                    return;
                };
                if let Some(notice) = review.phase.finish_send(result) {
                    review.notice = Some(notice.localized());
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn cancel_model_review(&mut self, id: RequestId, cx: &mut Context<Self>) {
        let Some(review) = self.current_review(&id, cx) else {
            return;
        };
        if matches!(review.phase, Phase::Cancelling { .. }) {
            return;
        }
        let Some(ticket) = review.phase.begin_cancel() else {
            self.model_review = None;
            cx.notify();
            return;
        };
        review.notice = None;
        let job = review_job(cx, review.origin.clone().cancel(ticket));
        cx.spawn(async move |rail, cx| {
            let result = job.await;
            let _ = rail.update(cx, |rail, cx| {
                let Some(review) = rail.current_review(&id, cx) else {
                    return;
                };
                if let Some(notice) = review.phase.finish_cancel(result) {
                    review.notice = Some(notice.localized());
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_model_review(&self, cx: &mut Context<Self>) -> AnyElement {
        let review = self.model_review.as_ref().expect("open review");
        let id = &review.id;
        let target = review
            .target
            .and_then(|index| review.choices.get(index))
            .map(target_name);
        let choosing = matches!(review.phase, Phase::Choosing);
        let ready = matches!(review.phase, Phase::Ready(..));
        let stopped = review.phase.close_only();
        let status = match &review.phase {
            Phase::Loading => text(
                "Loading configured models…",
                "正在加载已配置的模型…",
                "正在載入已設定的模型…",
            ),
            Phase::Choosing => text("Changes to review", "待审查的更改", "待審查的變更"),
            Phase::Previewing => text(
                "Preparing request preview…",
                "正在生成请求预览…",
                "正在產生請求預覽…",
            ),
            Phase::Ready(..) => text(
                "Full request · not sent",
                "完整请求正文 · 尚未发送",
                "完整請求正文 · 尚未送出",
            ),
            Phase::Sending(_) => text(
                "Sending. Cancelling may not prevent a charge.",
                "发送中。此时取消仍可能产生费用。",
                "傳送中。此時取消仍可能產生費用。",
            ),
            Phase::Cancelling { .. } | Phase::AwaitingCancel => {
                text("Cancelling…", "正在取消…", "正在取消…")
            }
            Phase::CancelFailed { .. } => {
                text("Cancellation not confirmed", "尚未确认取消", "尚未確認取消")
            }
            Phase::AwaitingSend { .. } | Phase::Finished => {
                text("Review status", "审查状态", "審查狀態")
            }
        };
        let mut panel = v_flex().flex_1().min_h_0().min_w_0().p_3().gap_2().child(
            div()
                .font_medium()
                .text_sm()
                .child(text("Model review", "模型审查", "模型審查")),
        );
        if choosing {
            let labels: Vec<_> = review.choices.iter().map(target_name).collect();
            let rail = cx.weak_entity();
            let picker_id = id.clone();
            panel = panel.child(
                Button::new("review-target")
                    .small()
                    .outline()
                    .w_full()
                    .label(target.unwrap_or_else(|| {
                        text(
                            "Choose provider and model",
                            "选择供应商和模型",
                            "選擇供應商和模型",
                        )
                        .to_owned()
                    }))
                    .dropdown_menu(move |mut menu, _, _| {
                        for (index, label) in labels.iter().enumerate() {
                            let rail = rail.clone();
                            let id = picker_id.clone();
                            menu = menu.item(PopupMenuItem::new(label.clone()).on_click(
                                move |_, _, cx| {
                                    let _ = rail.update(cx, |rail, cx| {
                                        if let Some(review) = rail.current_review(&id, cx)
                                            && matches!(review.phase, Phase::Choosing)
                                        {
                                            review.target = Some(index);
                                            review.notice = None;
                                            cx.notify();
                                        }
                                    });
                                },
                            ));
                        }
                        menu
                    }),
            );
        } else if let Some(target) = target {
            panel = panel.child(div().text_sm().child(target));
        }
        panel = panel.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(status),
        );
        let row_count = review.selection.len();
        if review.selection.has_untracked
            && matches!(
                review.phase,
                Phase::Choosing | Phase::Loading | Phase::Finished
            )
        {
            panel = panel.child(div().text_xs().text_color(cx.theme().muted_foreground).child(text(
                "Untracked files are hidden. Stage them manually, then reopen review to include them.",
                "未跟踪文件不在列表中。要审查这些文件，请手动暂存后重新打开审查。",
                "未追蹤檔案不在清單中。要審查這些檔案，請手動暫存後重新開啟審查。",
            )));
        }
        if choosing && row_count > 0 {
            let list_id = id.clone();
            panel = panel.child(
                uniform_list(
                    "review-file-sides",
                    row_count,
                    cx.processor(move |rail, range: Range<usize>, _, cx| {
                        let Some(review) = rail
                            .model_review
                            .as_ref()
                            .filter(|review| review.id == list_id)
                        else {
                            return Vec::new();
                        };
                        range
                            .filter_map(|index| {
                                let (path, staged, checked) = review.selection.row(index)?;
                                let side = if staged {
                                    text("Staged", "已暂存", "已暫存")
                                } else {
                                    text("Unstaged", "未暂存", "未暫存")
                                };
                                let label = format!("{side} · {path}");
                                let id = list_id.clone();
                                Some(
                                    Checkbox::new(SharedString::from(format!(
                                        "model-review-side-{index}"
                                    )))
                                    .small()
                                    .w_full()
                                    .h(px(28.0))
                                    .label(label)
                                    .checked(checked)
                                    .on_click(cx.listener(move |rail, checked: &bool, _, cx| {
                                        if let Some(review) = rail.current_review(&id, cx)
                                            && matches!(review.phase, Phase::Choosing)
                                        {
                                            review.selection.check(index, *checked);
                                            review.notice = None;
                                            cx.notify();
                                        }
                                    }))
                                    .into_any_element(),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .min_h_0(),
            );
        } else if choosing && !review.selection.has_untracked {
            panel = panel.child(div().text_sm().child(text(
                "No changes. Reopen after refreshing Git status.",
                "没有更改。刷新 Git 状态后再打开。",
                "沒有變更。重新整理 Git 狀態後再開啟。",
            )));
        } else if let Phase::Ready(_, body) = &review.phase {
            panel = panel.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_scrollbar()
                    .child(
                        div()
                            .flex_none()
                            .min_w_full()
                            .whitespace_nowrap()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .child(body.clone()),
                    ),
            );
        }
        if let Some(notice) = &review.notice {
            panel = panel.child(div().text_sm().child(notice.clone()));
        }
        let cancel_id = id.clone();
        let mut actions = h_flex().flex_none().flex_wrap().gap_2().child(
            Button::new("model-review-cancel")
                .small()
                .outline()
                .label(if stopped {
                    text("Close", "关闭", "關閉")
                } else if matches!(review.phase, Phase::CancelFailed { .. }) {
                    text("Retry cancel", "重试取消", "重試取消")
                } else {
                    text("Cancel", "取消", "取消")
                })
                .disabled(matches!(review.phase, Phase::Cancelling { .. }))
                .on_click(cx.listener(move |rail, _, _, cx| {
                    rail.cancel_model_review(cancel_id.clone(), cx)
                })),
        );
        if matches!(
            review.phase,
            Phase::Cancelling { .. } | Phase::CancelFailed { .. }
        ) {
            let dismiss = id.clone();
            actions = actions.child(
                Button::new("dismiss-review")
                    .small()
                    .ghost()
                    .label(text("Close", "关闭", "關閉"))
                    .on_click(cx.listener(move |rail, _, _, cx| {
                        if rail.current_review(&dismiss, cx).is_some_and(|review| {
                            matches!(
                                review.phase,
                                Phase::Cancelling { .. } | Phase::CancelFailed { .. }
                            )
                        }) {
                            rail.model_review = None;
                            cx.notify();
                        }
                    })),
            );
        }
        if choosing || ready {
            let confirm = id.clone();
            actions = actions.child(
                Button::new(if choosing {
                    "model-review-preview"
                } else {
                    "model-review-confirm"
                })
                .small()
                .primary()
                .label(if choosing {
                    text("Preview request", "预览请求", "預覽請求")
                } else {
                    text("Confirm and send", "确认发送", "確認送出")
                })
                .on_click(cx.listener(
                    move |rail, event: &gpui::ClickEvent, _, cx| {
                        if choosing {
                            rail.preview_model_review(confirm.clone(), cx);
                        } else if event.click_count() <= 1 {
                            rail.send_model_review(confirm.clone(), cx);
                        }
                    },
                )),
            );
        }
        panel.child(actions).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibex_backend::DisconnectedBackend;

    #[test]
    fn origin_requires_the_same_session_workspace_generation_and_backend() {
        let original = Origin {
            session: VibexSessionId::new(),
            workspace: WorkspaceId::new(),
            generation: 1,
            backend: DisconnectedBackend::facade(),
        };
        assert!(original.matches(&original.clone()));
        for switched in [
            Origin {
                session: VibexSessionId::new(),
                ..original.clone()
            },
            Origin {
                workspace: WorkspaceId::new(),
                ..original.clone()
            },
            Origin {
                generation: 2,
                ..original.clone()
            },
            Origin {
                backend: DisconnectedBackend::facade(),
                ..original.clone()
            },
        ] {
            assert!(!original.matches(&switched));
        }
    }
}
