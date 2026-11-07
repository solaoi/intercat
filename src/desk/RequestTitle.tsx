import type { Utterance } from '../bridge';
import { PrivacyText } from './PrivacyText';

const segmenter = new Intl.Segmenter('en', { granularity: 'grapheme' });
export function textLength(text: string) { return Array.from(segmenter.segment(text)).length; }

export function generatedTitle(word: Utterance) {
  const title = word.decision?.description?.title?.trim();
  return title && !/[\p{Cc}]/u.test(title) && textLength(title) <= 30 ? title : undefined;
}

export function requestTitle(word: Utterance) {
  return generatedTitle(word) ?? word.text;
}

export function GeneratedRequestTitle({ word }: { word: Utterance }) {
  const title = generatedTitle(word);
  return title ? <h3 className="request-generated-title"><PrivacyText text={title} interactive={false} /></h3> : null;
}
