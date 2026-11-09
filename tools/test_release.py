"""Tests for release.py. Run: python tools/test_release.py"""
import unittest

from release import level_of, next_version


class LevelOf(unittest.TestCase):
    def test_types(self):
        self.assertEqual(level_of("feat(snap): bearing"), "minor")
        self.assertEqual(level_of("fix: crash"), "patch")
        self.assertEqual(level_of("perf(core): faster"), "patch")
        for t in ("docs", "build", "ci", "chore", "test", "refactor", "style"):
            self.assertEqual(level_of(f"{t}: x"), "none")

    def test_breaking(self):
        self.assertEqual(level_of("feat(core)!: new format"), "major")
        self.assertEqual(level_of("fix!: drop field"), "major")
        self.assertEqual(level_of("docs: x\n\nBREAKING CHANGE: gone"), "major")
        self.assertEqual(level_of("docs: x\n\nBREAKING-CHANGE: gone"), "major")

    def test_not_conventional(self):
        self.assertEqual(level_of("Update readme"), "none")
        self.assertEqual(level_of(""), "none")
        self.assertEqual(level_of("feature: x"), "none")

    def test_release_commit_never_releases(self):
        self.assertEqual(level_of("chore(release): v0.2.0"), "none")


class NextVersion(unittest.TestCase):
    def test_zero_x_cargo_style(self):
        self.assertEqual(next_version((0, 1, 0), ["feat: a"]), (0, 1, 1))
        self.assertEqual(next_version((0, 1, 0), ["fix: a"]), (0, 1, 1))
        self.assertEqual(next_version((0, 1, 4), ["feat!: a"]), (0, 2, 0))

    def test_one_x_semver(self):
        self.assertEqual(next_version((1, 2, 3), ["fix: a"]), (1, 2, 4))
        self.assertEqual(next_version((1, 2, 3), ["feat: a", "fix: b"]), (1, 3, 0))
        self.assertEqual(next_version((1, 2, 3), ["fix!: a", "feat: b"]), (2, 0, 0))

    def test_highest_level_wins(self):
        self.assertEqual(next_version((0, 3, 0), ["fix: a", "feat!: b", "docs: c"]), (0, 4, 0))

    def test_no_release(self):
        self.assertIsNone(next_version((0, 1, 0), ["docs: a", "chore: b", "ci: c"]))
        self.assertIsNone(next_version((0, 1, 0), []))


if __name__ == "__main__":
    unittest.main()
