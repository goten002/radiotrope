//! The macOS menu bar (ui/platform/macos/window.slint) has the items of the
//! menu bar in the window (ui/menu-bar.slint): the same names in the same
//! order, each sending the command the window's item runs, with a check
//! mark where the window's has one and the keys the window's hint shows.
//! What macOS does differently is written out here (`as_on_macos`), so a
//! change to one file that isn't made in the other fails, on every system:
//! the two files are compared as text, since the macOS one is only compiled
//! there.

use std::collections::BTreeMap;

const WINDOW_BAR: &str = include_str!("../ui/menu-bar.slint");
const MACOS_BAR: &str = include_str!("../ui/platform/macos/window.slint");

/// An element of a .slint file, with what the comparison needs of it
#[derive(Debug, Default)]
struct Element {
    kind: String,
    /// `if <this>: Kind {`
    condition: String,
    /// `for x in <this>: Kind {`
    model: String,
    /// Its own property lines, by name: `label` -> `root.x ? "A" : "B"`
    properties: BTreeMap<String, String>,
    /// Everything written inside it that is not a child element (its
    /// properties and handlers)
    text: String,
    children: Vec<Element>,
}

/// A string literal's text, and the line without its literals (a `{` in a
/// string is not a block)
fn literals(line: &str) -> (Vec<String>, String) {
    let mut found = Vec::new();
    let mut rest = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            rest.push(c);
            continue;
        }
        let mut literal = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => {
                    literal.push(c);
                    literal.extend(chars.next());
                }
                _ => literal.push(c),
            }
        }
        found.push(literal);
        rest.push_str("\"\"");
    }
    (found, rest)
}

/// `[name := ][if cond: |for x in model: ]Kind {` -> (condition, model, kind)
fn element_start(code: &str) -> Option<(String, String, String)> {
    let head = code.trim().strip_suffix('{').or_else(|| {
        let code = code.trim();
        code.strip_suffix("{}").or_else(|| code.strip_suffix("{ }"))
    })?;
    let head = head.trim();
    let head = match head.split_once(" := ") {
        Some((name, rest)) if !name.contains(' ') => rest,
        _ => head,
    };
    let (condition, model, kind) = if let Some(rest) = head.strip_prefix("if ") {
        let (condition, kind) = rest.rsplit_once(": ")?;
        (condition, "", kind)
    } else if let Some(rest) = head.strip_prefix("for ") {
        let (repeat, kind) = rest.rsplit_once(": ")?;
        let (_, model) = repeat.split_once(" in ")?;
        ("", model, kind)
    } else {
        ("", "", head)
    };
    let is_kind = kind.starts_with(|c: char| c.is_ascii_uppercase())
        && kind.chars().all(|c| c.is_ascii_alphanumeric());
    is_kind.then(|| (condition.to_string(), model.to_string(), kind.to_string()))
}

/// The elements of a .slint file as a tree (its components at the top)
fn parse(source: &str) -> Element {
    // The elements being read, and for each how many blocks that are not
    // elements (handlers, functions, `if` statements) are open inside it
    let mut open: Vec<(Element, usize)> = vec![(Element::default(), 0)];
    for line in source.lines() {
        let (_, code) = literals(line);
        let code = code.split("//").next().unwrap_or_default().to_string();
        let line = line.split("//").next().unwrap_or_default();
        if code.trim().is_empty() {
            continue;
        }
        let in_block = open.last().is_some_and(|(_, blocks)| *blocks > 0);
        if !in_block {
            let declared = code.trim().strip_prefix("export ").unwrap_or(code.trim());
            if declared.starts_with("component ") || declared.starts_with("global ") {
                // Not an element of that kind: its declaration
                let kind = format!(
                    "{} declared",
                    declared.split(' ').nth(1).unwrap_or_default()
                );
                open.push((
                    Element {
                        kind,
                        ..Element::default()
                    },
                    0,
                ));
                continue;
            }
            if let Some((condition, model, kind)) = element_start(&code) {
                let element = Element {
                    kind,
                    condition,
                    model,
                    ..Element::default()
                };
                if code.trim().ends_with('{') {
                    open.push((element, 0));
                } else {
                    open.last_mut().unwrap().0.children.push(element);
                }
                continue;
            }
        }
        // A line of the element itself: a property, or (part of) a block
        let (element, blocks) = open.last_mut().unwrap();
        if *blocks == 0 {
            if let Some((name, value)) = line.trim().split_once(": ") {
                if !name.contains(' ') {
                    element
                        .properties
                        .insert(name.to_string(), value.trim_end_matches(';').to_string());
                }
            }
        }
        element.text.push_str(line.trim());
        element.text.push('\n');
        for c in code.chars() {
            let (_, blocks) = open.last_mut().unwrap();
            match c {
                '{' => *blocks += 1,
                '}' if *blocks > 0 => *blocks -= 1,
                '}' => {
                    let (done, _) = open.pop().unwrap();
                    open.last_mut()
                        .expect("a `}` closes an element")
                        .0
                        .children
                        .push(done);
                }
                _ => {}
            }
        }
    }
    assert_eq!(open.len(), 1, "every element is closed");
    open.pop().unwrap().0
}

fn find<'a>(element: &'a Element, kind: &str, found: &mut Vec<&'a Element>) {
    if element.kind == kind {
        found.push(element);
    }
    for child in &element.children {
        find(child, kind, found);
    }
}

/// A menu's row, as both files have it
#[derive(Debug, Clone, PartialEq)]
enum Row {
    Separator,
    Item {
        /// The texts in its name: `["Stop Recording", "Start Recording"]`
        names: Vec<String>,
        /// One per value of this list
        repeated: String,
        check: bool,
        /// `MenuCommand.<this>` that it sends; "" for none
        command: String,
        /// `Control + O`, `F1`; "" for no keys
        keys: String,
        /// Its rows, for a group or submenu
        rows: Vec<Row>,
    },
}

fn item(name: &str, command: &str) -> Row {
    Row::Item {
        names: vec![name.to_string()],
        repeated: String::new(),
        check: false,
        command: command.to_string(),
        keys: String::new(),
        rows: Vec::new(),
    }
}

/// `MenuCommand.<name>` in an element's own text
fn command(element: &Element) -> String {
    let mut commands: Vec<&str> = element
        .text
        .split("MenuCommand.")
        .skip(1)
        .map(|rest| {
            rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .next()
                .unwrap_or_default()
        })
        .collect();
    commands.dedup();
    assert!(commands.len() <= 1, "one command for an item: {commands:?}");
    commands.first().copied().unwrap_or_default().to_string()
}

/// As Slint shows them: none first or last, never two together
fn tidy(rows: Vec<Row>) -> Vec<Row> {
    let mut tidy: Vec<Row> = Vec::new();
    for row in rows {
        if row == Row::Separator && matches!(tidy.last(), None | Some(Row::Separator)) {
            continue;
        }
        tidy.push(row);
    }
    if tidy.last() == Some(&Row::Separator) {
        tidy.pop();
    }
    tidy
}

/// What macOS doesn't have (`Platform` in ui/platform/desktop/wifi.slint,
/// with `macos` set)
const NOT_ON_MACOS: &[&str] = &[
    "Platform.has-wifi",
    "Platform.has-power-off",
    "Platform.has-frame-choice",
    "Platform.has-minimize-to-tray",
    "Platform.has-start-at-login",
];

/// The element is shown on macOS: its condition asks for nothing that
/// macOS lacks (`!Platform.has-power-off` asks for the lack)
fn on_macos(element: &Element) -> bool {
    NOT_ON_MACOS.iter().all(|flag| {
        element
            .condition
            .match_indices(flag)
            .all(|(at, _)| element.condition[..at].ends_with('!'))
    })
}

/// The window's bar: the rows of a menu's pane, as shown on macOS
fn window_rows(elements: &[Element]) -> Vec<Row> {
    let mut rows = Vec::new();
    // The group whose rows the next layout holds was left out
    let mut group_shown = true;
    for element in elements {
        let shown = on_macos(element);
        let names = |property: &str| literals(&element.properties[property]).0;
        match element.kind.as_str() {
            "MenuSeparator" if shown => rows.push(Row::Separator),
            "MenuItem" | "CheckMenuItem" if shown => {
                let keys = element
                    .properties
                    .get("shortcut")
                    .map(|shortcut| {
                        let action = shortcut
                            .split("ShortcutAction.")
                            .nth(1)
                            .expect("a shortcut names its action")
                            .trim_end_matches([')', ' ']);
                        let action = action.split(')').next().unwrap_or_default();
                        crate::shortcuts::native_menu_keys(action)
                    })
                    .unwrap_or_default();
                // One row per value has no name of its own to compare
                let names = if element.model.is_empty() {
                    names("label")
                } else {
                    Vec::new()
                };
                rows.push(Row::Item {
                    names,
                    repeated: element.model.clone(),
                    check: element.kind == "CheckMenuItem",
                    command: command(element),
                    keys,
                    rows: Vec::new(),
                });
            }
            "GroupMenuItem" => {
                group_shown = shown;
                if shown {
                    rows.push(item(&names("label")[0], ""));
                }
            }
            // The Theme row's three choices
            "ThemeMenuRow" => {
                let mut theme = item("Theme", "");
                if let Row::Item { rows, .. } = &mut theme {
                    *rows = THEMES
                        .iter()
                        .map(|name| Row::Item {
                            names: vec![name.to_string()],
                            repeated: String::new(),
                            check: true,
                            command: String::new(),
                            keys: String::new(),
                            rows: Vec::new(),
                        })
                        .collect();
                }
                rows.push(theme);
            }
            // A group's rows, under it
            "VerticalLayout" if group_shown => {
                let inner = tidy(window_rows(&element.children));
                match rows.last_mut() {
                    Some(Row::Item { rows, .. }) => *rows = inner,
                    other => panic!("rows without their group: {other:?}"),
                }
            }
            // A note under greyed rows is no row
            _ => {}
        }
    }
    rows
}

const THEMES: [&str; 3] = ["System", "Dark", "Light"];

/// The macOS bar: the rows of a `Menu`
fn macos_rows(elements: &[Element]) -> Vec<Row> {
    let mut rows = Vec::new();
    for element in elements {
        // Made again to set its check mark right: not a list of rows
        let repeated = if element.model.contains("root.marks") {
            String::new()
        } else {
            element.model.clone()
        };
        let title = || literals(&element.properties["title"]).0;
        match element.kind.as_str() {
            "MenuSeparator" => rows.push(Row::Separator),
            // A line to read (the zoom's note, who is connected)
            "MenuItem"
                if element.properties.get("enabled").map(String::as_str) == Some("false") => {}
            "MenuItem" => {
                let keys = element
                    .properties
                    .get("shortcut")
                    .map(|keys| {
                        keys.trim_start_matches("@keys(")
                            .trim_end_matches(')')
                            .to_string()
                    })
                    .unwrap_or_default();
                let names = if repeated.is_empty() {
                    title()
                } else {
                    Vec::new()
                };
                rows.push(Row::Item {
                    names,
                    repeated,
                    check: element.properties.contains_key("checkable"),
                    command: command(element),
                    keys,
                    rows: Vec::new(),
                });
            }
            "Menu" => {
                let mut menu = item(&title()[0], "");
                if let Row::Item { rows, .. } = &mut menu {
                    *rows = tidy(macos_rows(&element.children));
                }
                rows.push(menu);
            }
            other => panic!("{other} in a menu"),
        }
    }
    rows
}

/// A menu of the window's bar as macOS has it: Quit and About are in the
/// application menu
fn as_on_macos(rows: Vec<Row>) -> Vec<Row> {
    let moved = |row: &Row| {
        matches!(row, Row::Item { command, names, .. }
            if command == "quit" || (command == "about" && names == &["About Radiotrope"]))
    };
    tidy(rows.into_iter().filter(|row| !moved(row)).collect())
}

fn name(row: &Row) -> String {
    match row {
        Row::Separator => "-".to_string(),
        Row::Item {
            names, repeated, ..
        } if names.is_empty() => format!("each of {repeated}"),
        Row::Item { names, .. } => names.join(" / "),
    }
}

fn titles(rows: &[Row]) -> Vec<String> {
    rows.iter().map(name).collect()
}

/// The menus of the window's bar, by title, in the bar's order
fn window_menus() -> Vec<(String, Vec<Row>)> {
    let source = parse(WINDOW_BAR);
    let mut found = Vec::new();
    find(&source, "MenuTitle", &mut found);
    let order: Vec<String> = found
        .iter()
        .map(|title| literals(&title.properties["label"]).0[0].clone())
        .collect();
    let mut panes = Vec::new();
    find(&source, "MenuPane", &mut panes);
    order
        .into_iter()
        .map(|title| {
            let pane = panes
                .iter()
                .find(|pane| literals(&pane.properties["label"]).0 == [title.as_str()])
                .unwrap_or_else(|| panic!("no pane for {title}"));
            let rows = pane
                .children
                .iter()
                .filter(|child| !["TouchArea", "PopupCard"].contains(&child.kind.as_str()));
            let rows: Vec<&Element> = rows.collect();
            assert_eq!(rows.len(), pane.children.len(), "{title}: only rows");
            (title, tidy(window_rows(&pane.children)))
        })
        .collect()
}

/// The menus of the macOS bar, by title, in the bar's order
fn macos_menus() -> Vec<(String, Vec<Row>)> {
    let source = parse(MACOS_BAR);
    let mut bars = Vec::new();
    find(&source, "MenuBar", &mut bars);
    assert_eq!(bars.len(), 1, "one menu bar");
    // Only where the system has a menu bar (not the headless renderers)
    assert_eq!(bars[0].condition, "NativeMenu.active");
    macos_rows(&bars[0].children)
        .into_iter()
        .map(|menu| match menu {
            Row::Item { names, rows, .. } => (names[0].clone(), rows),
            Row::Separator => panic!("a separator in the bar"),
        })
        .collect()
}

#[test]
fn the_parser_reads_elements_properties_and_handlers() {
    let tree = parse(
        r#"
        component Bar inherits Rectangle {
            // A comment { with a brace
            function run(command: MenuCommand) {
                if command == MenuCommand.quit { root.quit(); }
            }
            pane := MenuPane {
                label: "Open";
                if !Platform.has-power-off: MenuItem {
                    label: root.asked ? "Press Again \{root.n}" : "Quit";
                    clicked => {
                        if root.asked {
                            root.run(MenuCommand.quit);
                        } else {
                            root.asked = true;
                        }
                    }
                }
                MenuSeparator {}
                for percent[i] in Options.choices: CheckMenuItem {
                    label: percent + "%";
                }
            }
        }
        "#,
    );
    let mut panes = Vec::new();
    find(&tree, "MenuPane", &mut panes);
    let pane = panes[0];
    assert_eq!(pane.properties["label"], "\"Open\"");
    let kinds: Vec<&str> = pane.children.iter().map(|c| c.kind.as_str()).collect();
    assert_eq!(kinds, ["MenuItem", "MenuSeparator", "CheckMenuItem"]);
    let quit = &pane.children[0];
    assert_eq!(quit.condition, "!Platform.has-power-off");
    assert!(on_macos(quit));
    assert_eq!(
        literals(&quit.properties["label"]).0,
        ["Press Again \\{root.n}", "Quit"]
    );
    assert_eq!(command(quit), "quit");
    assert_eq!(pane.children[2].model, "Options.choices");
    let pi_only = Element {
        condition: "Platform.has-power-off".into(),
        ..Element::default()
    };
    assert!(!on_macos(&pi_only));
}

#[test]
fn the_macos_bar_has_the_menus_of_the_bar_in_the_window() {
    let window = window_menus();
    let macos = macos_menus();
    let window_titles: Vec<&str> = window.iter().map(|(title, _)| title.as_str()).collect();
    assert_eq!(window_titles, ["Open", "Stations", "View", "Tools", "Help"]);
    // The application menu first; Window before Help, which is last
    let macos_titles: Vec<&str> = macos.iter().map(|(title, _)| title.as_str()).collect();
    assert_eq!(
        macos_titles,
        [
            "Radiotrope",
            "Open",
            "Stations",
            "View",
            "Tools",
            "Window",
            "Help"
        ]
    );

    for (title, rows) in window {
        let expected = as_on_macos(rows);
        let (_, shown) = macos
            .iter()
            .find(|(menu, _)| *menu == title)
            .expect("the menu is in the macOS bar");
        assert_eq!(
            titles(shown),
            titles(&expected),
            "{title}: the same items in the same order"
        );
        for (shown, expected) in shown.iter().zip(&expected) {
            assert_eq!(shown, expected, "{title} > {}", name(expected));
        }
    }
}

// Not a row left out by mistake: what the comparison above reads from the
// window's bar is what the menus show (names as on the screen)
#[test]
fn the_bar_in_the_window_is_read_whole() {
    let menus = window_menus();
    let menu = |title: &str| {
        let (_, rows) = menus.iter().find(|(menu, _)| menu == title).unwrap();
        titles(rows)
    };
    assert_eq!(menu("Open"), ["Network Stream", "-", "Quit"]);
    assert_eq!(
        menu("Stations"),
        [
            "Search Stations",
            "Find in Favorites",
            "-",
            "Export Favorites",
            "Import Favorites",
            "-",
            "Report Play Count"
        ]
    );
    assert_eq!(
        menu("View"),
        [
            "Theme",
            "Accent Color",
            "-",
            "Visualizer",
            "Station Colors in Visualizer",
            "-",
            "Show Listening Stats",
            "Zoom",
            "-",
            "System Tray"
        ]
    );
    assert_eq!(
        menu("Tools"),
        [
            "Stop Recording / Start Recording",
            "Recordings",
            "-",
            "Sleep Timer",
            "Scheduler",
            "-",
            "Filters",
            "Equalizer",
            "Statistics",
            "-",
            "Connections"
        ]
    );
    assert_eq!(
        menu("Help"),
        [
            "Update Available: \\{Updates.available}",
            "-",
            "Keyboard Shortcuts",
            "-",
            "Check for Updates",
            "Check for Updates Automatically",
            "-",
            "About Radiotrope"
        ]
    );
    let (_, view) = menus.iter().find(|(menu, _)| menu == "View").unwrap();
    let group = |title: &str| {
        let rows = view.iter().find_map(|row| match row {
            Row::Item { names, rows, .. } if names == &[title] => Some(rows),
            _ => None,
        });
        titles(rows.unwrap())
    };
    assert_eq!(group("Theme"), THEMES);
    assert_eq!(group("Zoom"), ["each of ViewOptions.zoom-choices"]);
    // No Minimize to Tray and no Start at Login on macOS
    assert_eq!(
        group("System Tray"),
        ["Show Tray Icon", "Close to Tray", "Start Hidden in Tray"]
    );
    // The Theme row's choices are the picker's
    for theme in THEMES {
        assert!(
            WINDOW_BAR.contains(&format!("label: \"{theme}\"")),
            "{theme}"
        );
    }
}

#[test]
fn the_application_and_window_menus_are_the_systems() {
    let source = parse(MACOS_BAR);
    let mut menus = Vec::new();
    find(&source, "Menu", &mut menus);
    let menu = |title: &str| {
        menus
            .iter()
            .find(|menu| literals(&menu.properties["title"]).0 == [title])
            .unwrap_or_else(|| panic!("no {title} menu"))
    };
    let described: Vec<(String, String, String)> = menu("Radiotrope")
        .children
        .iter()
        .map(|item| {
            (
                literals(&item.properties["title"]).0[0].clone(),
                item.properties["role"].clone(),
                command(item),
            )
        })
        .collect();
    let described: Vec<(&str, &str, &str)> = described
        .iter()
        .map(|(title, role, command)| (title.as_str(), role.as_str(), command.as_str()))
        .collect();
    // Slint puts the items with a role in the application menu, in the
    // system's order with its separators; Quit gets Command+Q
    assert_eq!(
        described,
        [
            ("About Radiotrope", "about", "about"),
            ("Services", "services", ""),
            ("Hide Radiotrope", "hide", ""),
            ("Hide Others", "hide-others", ""),
            ("Show All", "show-all", ""),
            ("Quit Radiotrope", "quit", "quit"),
        ]
    );
    // Quit works with a dialog open too
    assert!(!menu("Radiotrope").children[5]
        .properties
        .contains_key("enabled"));

    let window = macos_rows(&menu("Window").children);
    let mut minimize = item("Minimize", "minimize");
    if let Row::Item { keys, .. } = &mut minimize {
        *keys = "Control + M".to_string();
    }
    assert_eq!(
        window,
        [
            minimize,
            item("Zoom", "maximize"),
            Row::Separator,
            item("Bring All to Front", ""),
        ]
    );
}

// The menu takes its keys before the window does: a key without Command
// would be taken from the text fields
#[test]
fn only_keys_with_command_and_f1_are_the_menus() {
    let source = parse(MACOS_BAR);
    let mut items = Vec::new();
    find(&source, "MenuItem", &mut items);
    let keys: Vec<&str> = items
        .iter()
        .filter_map(|item| item.properties.get("shortcut"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "@keys(Control + O)",
            "@keys(Control + F)",
            "@keys(Control + M)",
            "@keys(F1)"
        ]
    );
    assert_eq!(crate::shortcuts::native_menu_keys("record"), "");
    assert_eq!(crate::shortcuts::native_menu_keys("find-favorites"), "");
    assert_eq!(crate::shortcuts::native_menu_keys("search"), "Control + F");
    assert_eq!(crate::shortcuts::native_menu_keys("help"), "F1");
}

// Every command an item sends is one the bar in the window runs
#[test]
fn every_command_is_run() {
    let model = include_str!("../ui/menu-model.slint");
    let declared = model
        .split("export enum MenuCommand {")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("the commands");
    let declared: Vec<&str> = declared
        .lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    assert!(declared.len() > 20, "{declared:?}");
    let run = WINDOW_BAR
        .split("public function run(command: MenuCommand) {")
        .nth(1)
        .and_then(|rest| rest.split("out property <MenuState> state").next())
        .expect("MenuBar.run");
    for command in &declared {
        assert!(
            run.contains(&format!("if command == MenuCommand.{command} {{")),
            "MenuBar.run doesn't run {command}"
        );
        assert!(
            MACOS_BAR.contains(&format!("MenuCommand.{command})")),
            "the macOS bar never sends {command}"
        );
    }
}
