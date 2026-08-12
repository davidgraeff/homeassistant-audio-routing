<script lang="ts">
  import { theme, type Theme } from '../../lib/theme';

  const order: Theme[] = ['auto', 'light', 'dark'];
  const label: Record<Theme, string> = { auto: 'Auto', light: 'Light', dark: 'Dark' };
  const icon: Record<Theme, string> = { auto: '🌗', light: '☀️', dark: '🌙' };

  function cycle() {
    theme.update((t) => order[(order.indexOf(t) + 1) % order.length]);
  }
</script>

<button class="ghost" onclick={cycle} title="Theme (click to cycle)">
  {icon[$theme]} {label[$theme]}
</button>
