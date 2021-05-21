use crate::assert_js;

#[test]
fn string_literals() {
    assert_js!(
        r#"
fn go() {
  "Hello, Gleam!"
}
"#,
        r#""use strict";

function go() {
  return "Hello, Gleam!";
}
"#
    );
}

#[test]
fn string_patterns() {
    assert_js!(
        r#"
fn go(x) {
  let "Hello" = x
}
"#,
        r#""use strict";

function go(x) {
  if (x !== "Hello") throw new Error("Bad match");
  return x;
}
"#
    );
}

#[test]
fn equality() {
    assert_js!(
        r#"
fn go(a) {
  a == "ok"
  a != "ok"
  a == a
}
"#,
        r#""use strict";

function go(a) {
  a === "ok";
  a !== "ok";
  return a === a;
}
"#
    );
}
