// The message catalogue (design section 8, decision Q6). Every string a person reads is a key here, never a literal in a component.
// `{name}` marks a placeholder. Another language is another file that must satisfy `Catalogue`: a missing or extra key fails `svelte-check`.

export const en = {
  'app.title': 'Rusty admin',
  'app.loading': 'Loading…',
  'app.skip': 'Skip to the content',

  'nav.main': 'Main',
  'nav.domains': 'Domains',
  'nav.signOut': 'Sign out',

  'login.title': 'Sign in',
  'login.address': 'E-mail address',
  'login.password': 'Password',
  'login.submit': 'Sign in',
  'login.submitting': 'Signing in…',

  'setup.title': 'Set up the first administrator',
  'setup.intro': 'No administrator exists yet. Enter the setup token printed on the standard output of the admin service, then choose the address and password of the first platform administrator.',
  'setup.token': 'Setup token',
  'setup.address': 'E-mail address of the administrator',
  'setup.password': 'Password (at least {min} characters)',
  'setup.confirm': 'Repeat the password',
  'setup.submit': 'Create the administrator',
  'setup.submitting': 'Creating…',
  'setup.mismatch': 'The two passwords are not the same.',
  'setup.tooShort': 'The password must have at least {min} characters.',

  'home.title': 'Domains',
  'home.signedInAs': 'Signed in as {address}',
  'home.scopePlatform': 'Platform administrator',
  'home.scopeDomain': 'Domain administrator',

  'error.unauthorized': 'The address or the password is not right.',
  'error.invalid': 'Some of what was entered is not valid.',
  'error.forbidden': 'This action is not allowed.',
  'error.not_found': 'Not found.',
  'error.exists': 'This already exists.',
  'error.unsupported': 'The request could not be understood.',
  'error.throttled': 'Too many attempts. Try again in {seconds} seconds.',
  'error.throttledNow': 'Too many attempts. Try again later.',
  'error.busy': 'The service is busy. Try again in a moment.',
  'error.unavailable': 'This is not set up on the server.',
  'error.network': 'The server cannot be reached.',
  'error.unknown': 'Something went wrong.',
} as const;

export type MessageKey = keyof typeof en;
/** What every language file must satisfy. */
export type Catalogue = Record<MessageKey, string>;
