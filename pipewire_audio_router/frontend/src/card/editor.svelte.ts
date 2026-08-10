// State for the card's visual editor, mirroring model.svelte.ts's split: the
// custom element (card.ts) owns an instance and feeds it what Lovelace hands it;
// CardEditor.svelte only reads it and reports edits back through `onChange`.

import type { CardConfig } from './types';

export class EditorModel {
  config = $state<CardConfig>({});
  /** The full frontend `hass`, not the narrow `HassLike` the card itself needs:
   *  `ha-form` reads localization, locale and the config-entry registry off it.
   *  `null` until Lovelace sets it, which is why the form isn't rendered before. */
  hass = $state<Record<string, unknown> | null>(null);
  /** Set by the element to dispatch `config-changed`. */
  onChange: (config: CardConfig) => void = () => {};
}
