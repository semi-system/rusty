import { mount } from 'svelte';
import './app.css';
import App from './App.svelte';
import { App as State } from './lib/app.svelte';

const target = document.getElementById('app');
if (target === null) {
  throw new Error('the page has no #app element');
}
mount(App, { target, props: { app: new State() } });
