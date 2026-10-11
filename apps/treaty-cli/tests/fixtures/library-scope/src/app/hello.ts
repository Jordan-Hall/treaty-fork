import { Component } from '@angular/core';

@Component({
  selector: 'app-hello',
  standalone: true,
  templateUrl: './hello.html',
  styleUrl: './hello.scss',
})
export class Hello {
  readonly who = 'fixture';
}
