use super::*;

#[test]
fn agui_turns_offer_bar_chart_and_form() {
    let runner = attach(None);
    let names: Vec<String> = runner
        .tool_schemas()
        .iter()
        .filter_map(|schema| schema["function"]["name"].as_str().map(str::to_string))
        .collect();
    assert!(names.contains(&"bar_chart".to_string()), "{names:?}");
    assert!(names.contains(&"form".to_string()), "{names:?}");
}
