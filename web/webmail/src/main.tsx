import { render } from 'solid-js/web';
import './app.css';
import { createApi } from './api';
import { App } from './App';

const target = document.getElementById('app');
if (target === null) {
  throw new Error('the page has no #app element');
}
render(() => <App api={createApi()} />, target);
