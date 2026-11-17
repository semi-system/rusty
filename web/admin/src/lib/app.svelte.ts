// The state of the application: who is signed in, or which page the person has to see first. Runes (`$state`) make the view reactive.

import { Client } from '../api/client';
import type { Me } from '../api/types';
import { Live } from './live.svelte';

export type View = { kind: 'loading' } | { kind: 'signin' } | { kind: 'setup' } | { kind: 'in'; me: Me } | { kind: 'failed'; error: unknown };

export class App {
  view = $state<View>({ kind: 'loading' });
  readonly client: Client;
  /** The stream of hints that something changed (see live.svelte.ts). Opened by the console frame while someone is signed in. */
  readonly live: Live;

  constructor(client: Client = new Client(), live: Live = new Live()) {
    this.client = client;
    this.live = live;
  }

  /** Decide the first page: the application when a session exists, else the setup page while it is open, else sign-in. */
  async boot(): Promise<void> {
    try {
      const me = await this.client.me();
      if (me !== null) {
        this.view = { kind: 'in', me };
        return;
      }
      this.view = (await this.client.setupOpen()) ? { kind: 'setup' } : { kind: 'signin' };
    } catch (error) {
      this.view = { kind: 'failed', error };
    }
  }

  signedIn(me: Me): void {
    this.view = { kind: 'in', me };
  }

  async signOut(): Promise<void> {
    try {
      await this.client.signOut();
    } catch {
      // The server could not end the session (it is busy, or unreachable). The session cookie stays valid until it expires on its own, but this browser
      // stops using it: the person is shown the sign-in page.
    }
    this.view = { kind: 'signin' };
  }
}
