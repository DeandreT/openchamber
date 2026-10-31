import io
import json
import unittest
import urllib.error

import native_authentication as authentication


class AuthenticationFixtureTests(unittest.TestCase):
    def test_authentication_uses_its_explicit_fixture_mode(self):
        self.assertEqual(authentication.AuthenticationFixture.MODE, "--authentication")

    def test_only_an_explicit_locked_response_establishes_the_fixture(self):
        class Fixture:
            def __init__(self, status, body):
                self.status, self.body = status, body

            def get(self, path):
                if self.status == 200:
                    return self.body
                raise urllib.error.HTTPError("http://localhost/auth/session", self.status, "fixture", {},
                                             io.BytesIO(json.dumps(self.body).encode()))

        authentication.verify_password_requirement(Fixture(401, {"authenticated": False, "locked": True}))
        for status, body in [(200, {"authenticated": True}), (500, {"authenticated": False, "locked": True}),
                             (401, {"authenticated": True, "locked": True}), (401, {"authenticated": False})]:
            with self.subTest(status=status, body=body), self.assertRaises(RuntimeError):
                authentication.verify_password_requirement(Fixture(status, body))


if __name__ == "__main__":
    unittest.main()
