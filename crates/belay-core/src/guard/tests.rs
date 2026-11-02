use super::*;

#[test]
fn every_reason_has_a_rule() {
    let cases = [
        ("assert a == 1\nassert b == 2\n", "assert a == 1\n"),
        ("def test_a():\n    assert x\n", "def test_b():\n    pass\n"),
        (
            "def test_a():\n    assert x\n",
            "@pytest.mark.skip\ndef test_a():\n    assert x\n",
        ),
        ("assert x == 1\n", "assert True\n"),
        (
            "assert x == pytest.approx(0.3, rel=1e-6)\n",
            "assert x == pytest.approx(0.3, rel=0.1)\n",
        ),
    ];
    for (old, new) in cases {
        let Answer::Weakened { reasons } = assess(old, new) else {
            panic!("not weakened: {new}");
        };
        for r in reasons {
            assert_ne!(rule_of(&r), "other", "{r}");
        }
    }
    let e4 = expected_to_actual(
        "assert f() == 6",
        "assert f() == 5",
        &[("5".into(), "6".into())],
    );
    assert_eq!(rule_of(&e4.unwrap()), "expected_to_actual");
    assert_eq!(
        rule_of("the path is protected by .belay.toml (`x`)"),
        "protected"
    );
}

#[test]
fn test_paths() {
    for p in [
        "tests/test_api.py",
        "src/__tests__/a.js",
        "pkg/orders_test.go",
        "web/cart.spec.tsx",
        "conftest.py",
        "test_root.py",
        "crates/x/src/lib_test.rs",
        "specs/cart.js",
        "shop/tests.py",
        "src/tests.rs",
        "Shop.Tests/OrderTests.cs",
        "src/test/java/OrderTest.java",
        "spec/models/order_spec.rb",
    ] {
        assert!(is_test_path(p), "{p}");
    }
    for p in [
        "src/app.py",
        "src/testing.rs",
        "latest/notes.md",
        "src/contest.py",
    ] {
        assert!(!is_test_path(p), "{p}");
    }
}

#[test]
fn test_code_outside_test_paths_is_judged() {
    assert!(judges(
        "src/orders.rs",
        "#[test]\nfn total() { assert_eq!(t(), 6); }",
        ""
    ));
    assert!(judges(
        "app/models.py",
        "def test_total():\n    assert 1\n",
        ""
    ));
    assert!(!judges(
        "src/orders.rs",
        "fn total() -> u32 { 6 }",
        "fn total() -> u32 { 7 }"
    ));
}

#[test]
fn lifetime_before_a_later_quote_is_not_a_string() {
    let c = code_only("fn f<'a>(x: &'a str) { assert!(x == \"y\"); let c = 'z'; }");
    assert!(c.contains("&'a str"), "{c}");
    assert!(c.contains("assert!"), "{c}");
    let c = code_only("fn g<'a, 'b: 'a>(x: &'b str) { assert!(x.is_empty()) }");
    assert!(c.contains("assert!") && c.contains("&'b str"), "{c}");
    // Python strings stay strings.
    assert!(!code_only("f('a b', 'c')").contains("a b"));
}

#[test]
fn comments_and_strings_do_not_count() {
    let c =
        code_only("assert x  # assert y\nlet s = \"assert z\"; // expect(w)\n/* assert q */ ok");
    assert_eq!(c.matches("assert").count(), 1, "{c}");
    assert!(c.contains("\"s"), "{c}");
    // Rust attributes and lifetimes survive.
    let c = code_only("#[ignore]\nfn f<'a>(x: &'a str) { assert!(x.len() > 0) }");
    assert!(c.contains("#[ignore]") && c.contains("assert!("), "{c}");
    // Docstrings are strings.
    assert!(!code_only("\"\"\"\nassert x == 1\n\"\"\"\n").contains("assert"));
}

fn weakened(old: &str, new: &str) -> bool {
    matches!(assess(old, new), Answer::Weakened { .. })
}

/// T1: one weakening and one neutral case per rule and language.
#[test]
fn weakening_table() {
    let weak = [
        // Assertions removed or commented out.
        (
            "    assert r.status == 200\n    assert r.json['id'] == 42\n",
            "    assert r.status in (200, 500)\n",
            "assertions removed",
        ),
        (
            "    assert total(cart) == 6\n",
            "    # assert total(cart) == 6\n",
            "assertions removed",
        ),
        (
            "  expect(x).toBe(6);\n",
            "  // expect(x).toBe(6);\n",
            "assertions removed",
        ),
        (
            "    assert_eq!(a, 6);\n",
            "    // assert_eq!(a, 6);\n",
            "assertions removed",
        ),
        (
            "    if got != 6 { t.Errorf(\"got %d\", got) }\n",
            "    _ = got\n",
            "assertions removed",
        ),
        (
            "    assert.Equal(t, 6, got)\n",
            "    _ = got\n",
            "assertions removed",
        ),
        (
            "    expect(a).to.equal(1)\n",
            "    a\n",
            "assertions removed",
        ),
        (
            "    assert x == 1\n",
            "    \"\"\"assert x == 1\"\"\"\n",
            "assertions removed",
        ),
        // Skip markers.
        (
            "def test_a():\n",
            "@pytest.mark.skip\ndef test_a():\n",
            "skip marker",
        ),
        (
            "import pytest\n",
            "import pytest\npytestmark = pytest.mark.skip(reason=\"wip\")\n",
            "skip marker",
        ),
        (
            "def test_a():\n",
            "@mark.skip\ndef test_a():\n",
            "skip marker",
        ),
        (
            "def test_a():\n",
            "@skip('wip')\ndef test_a():\n",
            "skip marker",
        ),
        (
            "    def test_a(self):\n",
            "    @unittest.expectedFailure\n    def test_a(self):\n",
            "skip marker",
        ),
        (
            "    def test_a(self):\n        x()\n",
            "    def test_a(self):\n        self.skipTest('later')\n        x()\n",
            "skip marker",
        ),
        (
            "    x()\n",
            "    raise unittest.SkipTest('x')\n    x()\n",
            "skip marker",
        ),
        (
            "    pytest.param(1, 2),\n",
            "    pytest.param(1, 2, marks=pytest.mark.xfail),\n",
            "skip marker",
        ),
        (
            "    x()\n",
            "    pytest.skip('x')\n    x()\n",
            "skip marker",
        ),
        (
            "#[test]\nfn a() {}\n",
            "#[test]\n#[ignore = \"flaky\"]\nfn a() {}\n",
            "skip marker",
        ),
        (
            "#[test]\nfn a() {}\n",
            "#[test]\n#[ignore]\nfn a() {}\n",
            "skip marker",
        ),
        (
            "#[test]\nfn a() {}\n",
            "#[test]\n#[should_panic]\nfn a() {}\n",
            "should_panic",
        ),
        (
            "func TestA(t *testing.T) {\n",
            "func TestA(t *testing.T) {\n\tt.Skip(\"later\")\n",
            "skip marker",
        ),
        ("it('a', f)", "it.skip('a', f)", "skip marker"),
        (
            "it.each(rows)('a', f)",
            "it.skip.each(rows)('a', f)",
            "skip marker",
        ),
        ("test('a', f)", "test.fails('a', f)", "skip marker"),
        ("it('a', f)", "xit('a', f)", "skip marker"),
        (
            "@Test\nvoid a() {}\n",
            "@Test\n@Disabled\nvoid a() {}\n",
            "skip marker",
        ),
        ("it.only('a', f)", "it.skip('a', f)", "skip marker"),
        ("it('a', f)", "it.only('a', f)", "focus marker"),
        // Tests removed or renamed.
        (
            "def test_a():\n    x()\n",
            "def _test_a():\n    x()\n",
            "tests removed",
        ),
        (
            "#[test]\nfn a() { x() }\n",
            "fn a() { x() }\n",
            "tests removed",
        ),
        // Swallowed failures and early returns.
        (
            "    assert f() == 1\n",
            "    try:\n        assert f() == 1\n    except AssertionError:\n        pass\n",
            "swallows",
        ),
        (
            "    assert f() == 1\n",
            "    try:\n        assert f() == 1\n    except:\n        pass\n",
            "swallows",
        ),
        (
            "def test_a():\n    assert f() == 1\n",
            "def test_a():\n    return\n    assert f() == 1\n",
            "`return`",
        ),
        (
            "  expect(f()).toBe(1);\n",
            "  return;\n  expect(f()).toBe(1);\n",
            "`return`",
        ),
        ("run_tests\n", "run_tests || true\n", "|| true"),
        // Always passes.
        (
            "    assert total([1, 2, 3]) == 6",
            "    assert True",
            "always passes",
        ),
        (
            "    assert total([1, 2, 3]) == 6",
            "    assert True, \"todo\"",
            "always passes",
        ),
        (
            "    assert total([1, 2, 3]) == 6",
            "    assert 1 == 1",
            "always passes",
        ),
        (
            "    self.assertEqual(total(c), 6)",
            "    self.assertTrue(True)",
            "always passes",
        ),
        (
            "    assert total([1, 2, 3]) == 6",
            "    assert not False",
            "always passes",
        ),
        (
            "    assert_eq!(total(c), 6);",
            "    assert_eq!(1, 1);",
            "always passes",
        ),
        (
            "    assert_eq!(total(c), 6);",
            "    assert!(true, \"x\");",
            "always passes",
        ),
        (
            "    expect(total(c)).toBe(6);",
            "    expect(1).toBe(1);",
            "always passes",
        ),
        (
            "    expect(total(c)).toBe(6);",
            "    expect(true).toBeTruthy()",
            "always passes",
        ),
        (
            "    assert got == want",
            "    assert got == got",
            "always passes",
        ),
        // A raised time limit.
        (
            "    assert time.perf_counter() - start < 0.2\n",
            "    assert time.perf_counter() - start < 5\n",
            "time limit raised",
        ),
        // Looser tolerances.
        (
            "    assert x == pytest.approx(0.3, rel=1e-6)",
            "    assert x == pytest.approx(0.3, rel=0.1)",
            "tolerance raised",
        ),
        (
            "    assert x == pytest.approx(0.3, rel=0.5)",
            "    assert x == pytest.approx(0.3, rel=1.0)",
            "tolerance raised",
        ),
        (
            "def test_a():\n    if ready:\n        x()\n",
            "def test_a():\n    pytest.skip('later')\n    if ready:\n        x()\n",
            "skip marker",
        ),
        (
            "    expect(x).toBeCloseTo(0.3, 5);",
            "    expect(x).toBeCloseTo(0.3, 1);",
            "precision lowered",
        ),
        (
            "    self.assertAlmostEqual(x, 0.3, places=7)",
            "    self.assertAlmostEqual(x, 0.3, places=2)",
            "precision lowered",
        ),
        (
            "    assert abs(x - y) < tol, (tol=1e-9)",
            "    assert abs(x - y) < tol, (tol=1)",
            "tolerance raised",
        ),
    ];
    for (old, new, why) in weak {
        let reasons = match assess(old, new) {
            Answer::Weakened { reasons } => reasons,
            other => panic!("not flagged ({other:?}):\n{old}\n→\n{new}"),
        };
        assert!(
            reasons.iter().any(|r| r.contains(why)),
            "expected `{why}`, got {reasons:?}:\n{old}\n→\n{new}"
        );
    }
    let neutral = [
        // A tighter time limit is not a weaker test.
        (
            "    assert time.perf_counter() - start < 5\n",
            "    assert time.perf_counter() - start < 0.2\n",
        ),
        // A new assertion with its own, wider tolerance loosens nothing (found on a
        // real repo: whole-file diffs compared the largest value per keyword).
        (
            "    assert m == pytest.approx(50.0 - 15.0, abs=5.0)\n",
            "    assert m == pytest.approx(50.0 - 15.0, abs=5.0)\n    assert m == pytest.approx(50.0 - 90.0, abs=20.0)\n",
        ),
        (
            "    assert a == pytest.approx(1.0, abs=1.0)\n",
            "    assert a == pytest.approx(1.0, abs=1.0)\n    assert b == pytest.approx(-40.0, abs=20.0)\n",
        ),
        // Refactors, rewording, imports.
        (
            "from pandas.testing import assert_frame_equal\nassert_frame_equal(a, b)\n",
            "assert_frame_equal(a, b)\n",
        ),
        (
            "# check the assertion holds\nassert x\n",
            "# the assertion must hold\nassert x\n",
        ),
        ("assert a == 1", "assert a == 2"),
        (
            "    assert r.json['msg'] == \"assert ok\"\n",
            "    assert r.json['msg'] == \"assert fine\"\n",
        ),
        (
            "def test_a():\n    x()\n",
            "def test_a_renamed_clearly():\n    x()\n",
        ),
        ("it('a', f)", "it('a better name', f)"),
        ("    assert ok is True", "    assert ok is True and done"),
        ("    assert y", "    assert flag == True_value"),
        ("    assert y", "assert x"),
        (
            "    assert x == pytest.approx(0.3, rel=0.1)",
            "    assert x == pytest.approx(0.3, rel=1e-6)",
        ),
        (
            "    if x:\n        return\n    do()\n",
            "    if x:\n        return\n    do_more()\n",
        ),
        ("fn helper() -> u32 { 6 }\n", "fn helper() -> u32 { 7 }\n"),
        // A skip under a runtime condition: the test cannot run here.
        ("it('a', f)", "it.skipIf(ci)('a', f)"),
        (
            "def test_a():\n    x()\n",
            "@pytest.mark.skipif(not HAVE_GPU, reason='gpu')\ndef test_a():\n    x()\n",
        ),
        (
            "    def test_a(self):\n        x()\n",
            "    @unittest.skipUnless(HAVE_GPU, 'gpu')\n    def test_a(self):\n        x()\n",
        ),
        (
            "def test_a():\n    d = load()\n    x(d)\n",
            "def test_a():\n    try:\n        d = load()\n    except OSError:\n        pytest.skip('data unavailable')\n    x(d)\n",
        ),
        (
            "def test_a():\n    x()\n",
            "def test_a():\n    if not DATA.exists():\n        pytest.skip('no data')\n    x()\n",
        ),
        (
            "import pytest\n",
            "import pytest\nif not DATA.exists():\n    pytest.skip('no data', allow_module_level=True)\n",
        ),
        (
            "func TestA(t *testing.T) {\n\tx()\n}\n",
            "func TestA(t *testing.T) {\n\tif testing.Short() {\n\t\tt.Skip(\"slow\")\n\t}\n\tx()\n}\n",
        ),
        // A skipped test the edit adds takes no running test away.
        (
            "def test_a():\n    assert x()\n",
            "def test_a():\n    assert x()\n\n@pytest.mark.skip(reason='wip')\ndef test_b():\n    assert y()\n",
        ),
        // A loosened tolerance with its reason, or a small tweak.
        (
            "    assert torch.allclose(u, v, atol=1e-10)\n",
            "    assert torch.allclose(u, v, atol=1e-6)  # U was cast from float32\n",
        ),
        (
            "    assert torch.allclose(u, v, atol=1e-10)\n",
            "    # U was cast from float32, so it is only this close\n    assert torch.allclose(u, v, atol=1e-6)\n",
        ),
        (
            "    assert x == pytest.approx(0.3, rel=1e-6)\n",
            "    assert x == pytest.approx(0.3, rel=2e-6)\n",
        ),
    ];
    for (old, new) in neutral {
        assert!(
            !weakened(old, new),
            "flagged:\n{old}\n→\n{new}\n{:?}",
            assess(old, new)
        );
    }
}

#[test]
fn expected_value_changed_to_actual() {
    let pairs = value_pairs("FAILED tests/t.py::test_total - assert 0 == 1");
    assert_eq!(pairs, [("0".to_string(), "1".to_string())]);
    let old = "def test_total():\n    assert total([]) == 1\n";
    let new = "def test_total():\n    assert total([]) == 0\n";
    assert!(expected_to_actual(old, new, &pairs)
        .unwrap()
        .contains("(1 → 0)"));
    // A different fix (the code, or another value) is not flagged.
    assert!(expected_to_actual(
        old,
        "def test_total():\n    assert total([]) == 1\n    assert x\n",
        &pairs
    )
    .is_none());
    let jest = value_pairs("expect(received).toBe(expected); Expected: 6; Received: 5");
    assert_eq!(jest, [("5".to_string(), "6".to_string())]);
    assert!(expected_to_actual("expect(t()).toBe(6);", "expect(t()).toBe(5);", &jest).is_some());
    let rust = value_pairs("assertion `left == right` failed; left: 5; right: 6");
    assert!(expected_to_actual("assert_eq!(t(), 6);", "assert_eq!(t(), 5);", &rust).is_some());
    let s = value_pairs("AssertionError: assert 'EUR' == 'USD'");
    assert!(expected_to_actual("assert c == 'USD'", "assert c == 'EUR'", &s).is_some());
    assert!(value_pairs("KeyError: 'total'").is_empty());
}

#[test]
fn neutral_and_strengthened() {
    assert_eq!(assess("assert a == 1", "assert a == 2"), Answer::Neutral);
    assert_eq!(
        assess("assert a", "assert a\nassert b"),
        Answer::Strengthened
    );
}

#[test]
fn a_new_file_weakens_nothing() {
    for new in [
        "@pytest.mark.skip\ndef test_a():\n    pass\n",
        "it.only('a', f)\n",
        "def test_a():\n    pytest.skip('x')\n",
    ] {
        assert!(!weakened("", new), "{new}");
        assert!(!weakened("\n", new), "{new}");
    }
}

#[test]
fn focus_is_exact_per_framework() {
    let py = "def test_a():\n    m.fit(x)\n    assert torch.allclose(a, b, atol=1e-6)\n";
    let py_new = format!("{py}\ndef fit(x):\n    return clean_only(x, 'test.only')\n\nfit(1)\n");
    assert!(
        !weakened_in("tests/test_a.py", py, &py_new),
        "{:?}",
        assess_path("tests/test_a.py", py, &py_new)
    );
    let js = "it('a', f)\nmodel.fit(x)\n";
    let js_new = "it('a', f)\nmodel.fit(x)\nfunction fit(x) {}\nconst y = obj.it.only;\n";
    assert!(!weakened_in("a.test.js", js, js_new));
    for (path, old, new) in [
        ("a.test.js", "it('a', f)", "it.only('a', f)"),
        ("a.test.ts", "describe('a', f)", "fdescribe('a', f)"),
        ("a.test.js", "it('a', f)", "fit('a', f)"),
        (
            "tests/test_a.py",
            "def test_a():\n",
            "@pytest.mark.only\ndef test_a():\n",
        ),
    ] {
        assert!(weakened_in(path, old, new), "{path}: {new}");
    }
    // Rust has no focus marker: `fit(` there is a function.
    assert!(!weakened_in(
        "tests/a.rs",
        "fn a() {}\n",
        "fn a() {}\nfn b() { fit(1); }\n"
    ));
}

fn weakened_in(path: &str, old: &str, new: &str) -> bool {
    matches!(assess_path(path, old, new), Answer::Weakened { .. })
}

#[test]
fn items_find_definitions_and_strip_moves() {
    let src = "use x;\n\n#[test]\nfn a() {\n    assert_eq!(f(\"}\"), 1);\n}\n\nfn helper<'a>(s: &'a str) -> &'a str {\n    s\n}\n";
    let found = items::items(src);
    let names: Vec<_> = found.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["a", "helper"]);
    assert!(src[found[0].span.clone()].starts_with("#[test]"));
    let moved: BTreeSet<_> = [found[0].key()].into_iter().collect();
    let rest = items::strip(src, &moved);
    assert!(
        !rest.contains("assert_eq") && rest.contains("fn helper"),
        "{rest}"
    );
    // The same body elsewhere, indented differently, has the same key.
    let other =
        "mod tests {\n    #[test]\n    fn a() {\n        assert_eq!(f(\"}\"), 1);\n    }\n}\n";
    assert!(items::items(other)
        .iter()
        .any(|i| i.key() == found[0].key()));
    let py = "class T:\n    @mark.slow\n    def test_b(self):\n        assert 1\n\n    def test_c(self):\n        assert 2\n";
    let found = items::items(py);
    let names: Vec<_> = found.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["test_b", "test_c"]);
    assert!(py[found[0].span.clone()].contains("@mark.slow"));
    let js = "describe('d', () => {\n  it('a', () => {\n    expect(f(')')).toBe(1);\n  });\n});\n";
    let names: Vec<_> = items::items(js).into_iter().map(|i| i.name).collect();
    assert_eq!(names, ["d", "a"]);
}
