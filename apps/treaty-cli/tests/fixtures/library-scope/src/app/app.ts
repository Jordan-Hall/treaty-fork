import { Component } from '@angular/core';
import { DecimalPipe, UpperCasePipe, AsyncPipe } from '@angular/common';
import { FormControl, FormControlDirective, FormsModule, ReactiveFormsModule } from '@angular/forms';
import { Hello } from './hello';

// Library imports the native build must resolve from the published partial declarations (TREATY-ISSUES T3/T9).
// Expected `dependencies` (Angular's order: directives first, then pipes, each in scope order):
//   FormControlDirective, DefaultValueAccessor, NgModel, Hello, DecimalPipe, UpperCasePipe, AsyncPipe.
// `FormControl` is a plain class and must not be listed.
@Component({
  selector: 'app-root',
  standalone: true,
  imports: [DecimalPipe, UpperCasePipe, AsyncPipe, FormControlDirective, FormsModule, ReactiveFormsModule, Hello],
  template: `<p>{{ 1 | number }} {{ 'x' | uppercase }} {{ x | async }}</p>
<input [formControl]="c" /><input [(ngModel)]="n" /><app-hello />`,
})
export class App {
  readonly c = new FormControl('');
  n = '';
  x = null;
}
