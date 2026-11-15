// Loading data for a page: one place that knows the three states, so every page shows a spinner-less "loading" line, the data, or the error the same way.

export type Loaded<T> = { status: 'loading' } | { status: 'ready'; data: T } | { status: 'error'; error: unknown };

export class Load<T> {
  state = $state<Loaded<T>>({ status: 'loading' });
  private readonly fetcher: () => Promise<T>;

  constructor(fetcher: () => Promise<T>) {
    this.fetcher = fetcher;
  }

  /** (Re)load. The previous data stays visible while the new data is on its way. */
  async run(): Promise<void> {
    try {
      this.state = { status: 'ready', data: await this.fetcher() };
    } catch (error) {
      this.state = { status: 'error', error };
    }
  }

  /** Replace the data without a request (after a change the server has already confirmed). */
  set(data: T): void {
    this.state = { status: 'ready', data };
  }
}
