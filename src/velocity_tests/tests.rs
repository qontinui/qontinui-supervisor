/// Definition of a single velocity test case.
#[allow(dead_code)]
pub struct TestCase {
    pub name: &'static str,
    pub page_url: &'static str,
    /// CSS-style element identifier to look for in the UI Bridge elements list.
    /// We search for this substring in element labels/ids/types.
    pub key_element: &'static str,
    pub api_endpoint: &'static str,
}

/// The 5 test pages we measure.
pub static TEST_CASES: &[TestCase] = &[
    TestCase {
        name: "Dashboard",
        page_url: "/",
        key_element: "project",
        api_endpoint: "/api/v1/projects/",
    },
    TestCase {
        name: "Settings",
        page_url: "/settings",
        key_element: "settings",
        api_endpoint: "/api/v1/auth/users/me",
    },
    TestCase {
        name: "Runs History",
        page_url: "/runs",
        key_element: "run",
        api_endpoint: "/api/v1/task-runs/?limit=10",
    },
    TestCase {
        name: "Runners",
        page_url: "/runners",
        key_element: "runner",
        api_endpoint: "/api/v1/runners/",
    },
    TestCase {
        name: "Build Tests",
        page_url: "/build/tests",
        key_element: "test",
        api_endpoint: "/api/v1/test-suites/",
    },
];
