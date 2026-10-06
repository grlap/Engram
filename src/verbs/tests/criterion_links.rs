use super::*;
use crate::{ObjectId, ProjectId, WorkId, WorkRunId};

fn page(total: usize) -> WorkCriterionLinksWindow {
    let locator = ObjectId::mint();
    WorkCriterionLinksWindow {
        short_ref: "w-123456789abc".into(),
        total,
        earlier: 0,
        project: ProjectId("hostile-\"🙂\u{202e}\u{1b}".into()),
        work: WorkId::new(),
        run: WorkRunId::new(),
        seal: ObjectId::mint(),
        rows: (1..=16)
            .map(|evidence_member| WorkCriterionLinkRow {
                criterion: 1,
                evidence_member,
                locator: locator.clone(),
                preview: Some("\"\\🙂\u{202e}\u{1b}\n".repeat(500)),
                preview_error_class: None,
            })
            .collect(),
    }
}

#[test]
fn criterion_links_budget_sheds_previews_then_rows_and_advances_the_emitted_member() {
    let page = page(17);
    let identities = render(&page, 16, false, 12288).unwrap();
    let receipt = fit_window(&page, 12288).unwrap();
    assert_eq!(
        receipt.value["criterion_links"],
        identities.value["criterion_links"]
    );
    assert_eq!(receipt.value["criterion_links_window"]["shown"], 16);
    assert!(!receipt.text().contains('\u{1b}'));
    let reserved = super::super::receipts::compact_receipt_json_bytes(
        &render(&page, 5, false, 1).unwrap().value,
    )
    .unwrap()
    .max(render(&page, 5, false, 1).unwrap().text().len() + 1)
        + 100;
    let fitted = fit_window(&page, reserved).unwrap();
    let shown = usize::try_from(
        fitted.value["criterion_links_window"]["shown"]
            .as_u64()
            .unwrap(),
    )
    .unwrap();
    assert!(shown > 0 && shown < 16);
    assert_eq!(
        fitted.value["criterion_links_window"]["after"],
        page.continuation(shown).unwrap().unwrap()
    );
    assert!(super::super::receipts::agent_receipt_fits(&fitted, reserved).unwrap());
    assert!(
        fit_window(&page, 1).is_err(),
        "never return an empty page with a remainder"
    );
}

#[test]
fn criterion_links_final_page_fits_without_assuming_cursor_size_monotonicity() {
    let page = page(16);
    let complete = render(&page, 16, false, 12288).unwrap();
    let budget = super::super::receipts::compact_receipt_json_bytes(&complete.value)
        .unwrap()
        .max(complete.text().len() + 1)
        + 20;
    let fitted = fit_window(&page, budget).unwrap();
    assert_eq!(fitted.value["criterion_links_window"]["shown"], 16);
    assert!(fitted.value["criterion_links_window"]["after"].is_null());
}
